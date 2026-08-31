use std::{error::Error, io, path::Path, time::Duration};

use decode::tools::{
    SandboxRoot,
    exec::{
        CommandApproval, ConfirmationDecision, ConfirmationReason, ExecError, ExecOptions,
        MAX_TRUSTED_STDIN_BYTES, ShellConfirmationMode, StrictAllowlistEntry,
        confirmation_decision, confirmation_decision_with_mode, confirmation_decision_with_options,
        execute_command, execute_trusted_direct,
    },
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn trusted_direct_program_receives_bounded_stdin_without_a_shell()
-> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let (program, args) = stdin_echo_program()?;
    let payload = b"hook-payload-line\n";

    let output = execute_trusted_direct(
        &sandbox,
        &program,
        &args,
        payload,
        Duration::from_secs(5),
        4_096,
        CancellationToken::new(),
    )
    .await?;

    assert!(output.contains("hook-payload-line"));
    Ok(())
}

#[tokio::test]
async fn trusted_direct_stdin_limit_is_checked_before_spawn() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let oversized = vec![b'x'; MAX_TRUSTED_STDIN_BYTES + 1];

    let result = execute_trusted_direct(
        &sandbox,
        &definitely_missing_absolute_program(),
        &[],
        &oversized,
        Duration::from_secs(1),
        1_024,
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(
        result,
        Err(ExecError::TrustedStdinTooLarge { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn trusted_direct_program_is_killed_on_timeout() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let (program, args) = slow_direct_program()?;

    let result = execute_trusted_direct(
        &sandbox,
        &program,
        &args,
        &[],
        Duration::from_millis(75),
        4_096,
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(ExecError::TimedOut { .. })));
    Ok(())
}

#[test]
fn every_command_requires_user_confirmation() {
    for command in [
        "cargo check",
        "cargo check --all-targets --all-features",
        "git status --short",
        "echo hello",
    ] {
        assert_eq!(
            confirmation_decision(command, false),
            ConfirmationDecision::RequiresUserConfirmation {
                reason: ConfirmationReason::PolicyRequired,
            }
        );
    }

    assert!(matches!(
        confirmation_decision("cargo check", true),
        ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::ModelRequested,
        }
    ));
}

#[test]
fn dangerous_cargo_arguments_are_not_auto_approved() {
    assert!(
        confirmation_decision("cargo check --target-dir=outside", false,).requires_confirmation()
    );

    assert!(
        confirmation_decision("cargo check --manifest-path=outside", false,)
            .requires_confirmation()
    );

    assert!(
        confirmation_decision("cargo check --config=build.rustc-wrapper=evil", false,)
            .requires_confirmation()
    );

    assert!(confirmation_decision("cargo check %MALICIOUS%", false,).requires_confirmation());
}

#[test]
fn forced_rules_override_model_false() {
    for command in [
        "rm -rf target",
        "sudo cargo check",
        "cargo check | tee build.log",
        "cargo check; rm -rf target",
        "git push --force origin main",
        "git reset --hard HEAD~1",
    ] {
        assert!(matches!(
            confirmation_decision(command, false),
            ConfirmationDecision::RequiresUserConfirmation {
                reason: ConfirmationReason::ForcedRule(_),
            }
        ));
    }
}

#[test]
fn strict_allowlist_is_small_and_never_contains_cargo() {
    assert_eq!(
        confirmation_decision_with_mode("whoami", false, ShellConfirmationMode::StrictAllowlist),
        ConfirmationDecision::AutoApproved
    );

    for command in [
        "cargo check",
        "cargo test",
        "whoami extra",
        "whoami | echo injected",
    ] {
        assert!(
            confirmation_decision_with_mode(command, false, ShellConfirmationMode::StrictAllowlist)
                .requires_confirmation()
        );
    }

    assert!(
        confirmation_decision_with_mode("whoami", true, ShellConfirmationMode::StrictAllowlist)
            .requires_confirmation()
    );
}

#[test]
fn configured_strict_allowlist_matches_only_the_exact_argv() -> Result<(), Box<dyn Error>> {
    let (program, args, command) = configured_read_only_command();
    let entry = StrictAllowlistEntry::new(program, args.iter().copied())?;
    let options = ExecOptions::default()
        .with_confirmation_mode(ShellConfirmationMode::StrictAllowlist)
        .with_strict_allowlist_entries([entry]);

    assert_eq!(
        confirmation_decision_with_options(command, false, &options),
        ConfirmationDecision::AutoApproved
    );
    assert!(
        confirmation_decision_with_options(&format!("{command} extra"), false, &options)
            .requires_confirmation()
    );
    Ok(())
}

#[test]
fn configurable_allowlist_rejects_build_shell_and_injection_entries() {
    for (program, args) in [
        ("cargo", vec!["check"]),
        ("cmake", vec!["--version"]),
        ("cmd.exe", vec!["/C", "whoami"]),
        ("python", vec!["--version"]),
        ("mv", vec!["source", "destination"]),
        ("fsutil", vec!["file", "createnew", "target", "1"]),
        (
            "certutil",
            vec!["-urlcache", "-f", "https://example.test", "x"],
        ),
    ] {
        assert!(matches!(
            StrictAllowlistEntry::new(program, args),
            Err(ExecError::InvalidStrictAllowlistEntry { .. })
        ));
    }

    assert!(matches!(
        StrictAllowlistEntry::new("whoami", ["|", "hostname"]),
        Err(ExecError::InvalidStrictAllowlistEntry { .. })
    ));

    let options =
        ExecOptions::default().with_confirmation_mode(ShellConfirmationMode::StrictAllowlist);
    for command in ["cargo check", "cargo build", "whoami | hostname"] {
        assert!(
            confirmation_decision_with_options(command, false, &options).requires_confirmation()
        );
    }
}

#[tokio::test]
async fn strict_allowlisted_command_runs_directly_without_approval() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let options =
        ExecOptions::default().with_confirmation_mode(ShellConfirmationMode::StrictAllowlist);

    let output = execute_command(
        &sandbox,
        "whoami",
        false,
        CommandApproval::NotGranted,
        options,
        CancellationToken::new(),
    )
    .await?;

    assert!(!output.trim().is_empty());
    Ok(())
}

#[tokio::test]
async fn configured_strict_allowlisted_argv_runs_directly_without_approval()
-> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let (program, args, command) = configured_read_only_command();
    let options = ExecOptions::default()
        .with_confirmation_mode(ShellConfirmationMode::StrictAllowlist)
        .with_strict_allowlist_entries([StrictAllowlistEntry::new(program, args.iter().copied())?]);

    let output = execute_command(
        &sandbox,
        command,
        false,
        CommandApproval::NotGranted,
        options,
        CancellationToken::new(),
    )
    .await?;

    assert!(!output.trim().is_empty());
    Ok(())
}

#[tokio::test]
async fn command_input_is_bounded_before_confirmation_or_spawn() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = "x".repeat(decode::tools::MAX_COMMAND_BYTES + 1);

    let result = execute_command(
        &sandbox,
        &command,
        false,
        CommandApproval::NotGranted,
        ExecOptions::default(),
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(ExecError::CommandTooLarge { .. })));
    Ok(())
}

#[tokio::test]
async fn even_safe_model_false_command_needs_approval() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = marker_command("must-not-exist.txt");

    let result = execute_command(
        &sandbox,
        &command,
        false,
        CommandApproval::NotGranted,
        ExecOptions::default(),
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(
        result,
        Err(ExecError::ConfirmationRequired { .. })
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());

    Ok(())
}

#[tokio::test]
async fn approval_is_bound_to_exact_command() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let actual = marker_command("must-not-exist.txt");
    let approval = CommandApproval::confirmed_for("different command", false);

    let result = execute_command(
        &sandbox,
        &actual,
        false,
        approval,
        ExecOptions::default(),
        CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(ExecError::ApprovalMismatch)));
    assert!(!root.path().join("must-not-exist.txt").exists());

    Ok(())
}

#[tokio::test]
async fn output_is_bounded_and_keeps_tail() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = long_output_command();

    let output = execute_command(
        &sandbox,
        &command,
        false,
        CommandApproval::confirmed_for(&command, false),
        ExecOptions::new(Duration::from_secs(10), 512),
        CancellationToken::new(),
    )
    .await?;

    assert!(output.len() <= 512);
    assert!(output.contains("[output truncated;"));
    assert!(output.trim_end().ends_with("TAIL"));

    Ok(())
}

#[tokio::test]
async fn timeout_kills_and_reaps_process() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = slow_command();

    let started = tokio::time::Instant::now();

    let result = execute_command(
        &sandbox,
        &command,
        false,
        CommandApproval::confirmed_for(&command, false),
        ExecOptions::new(Duration::from_millis(200), 1024),
        CancellationToken::new(),
    )
    .await;

    assert!(
        matches!(result, Err(ExecError::TimedOut { .. })),
        "unexpected timeout result: {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));

    Ok(())
}

#[tokio::test]
async fn cancellation_kills_running_process() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = slow_command();

    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();

    let task = tokio::spawn(async move {
        execute_command(
            &sandbox,
            &command,
            false,
            CommandApproval::confirmed_for(&command, false),
            ExecOptions::new(Duration::from_secs(10), 1024),
            worker_cancel,
        )
        .await
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let started = tokio::time::Instant::now();
    cancel.cancel();

    let joined = tokio::time::timeout(Duration::from_secs(3), task).await?;

    let result = joined?;

    assert!(
        matches!(result, Err(ExecError::Cancelled { .. })),
        "unexpected cancellation result: {result:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn normal_exit_kills_background_descendants() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = concat!(
        "(sleep 1; printf leaked > leaked-after-exit.txt) & ",
        "printf done"
    );

    let output = execute_command(
        &sandbox,
        command,
        false,
        CommandApproval::confirmed_for(command, false),
        ExecOptions::new(Duration::from_secs(5), 1024),
        CancellationToken::new(),
    )
    .await?;

    assert_eq!(output, "done");
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    assert!(!root.path().join("leaked-after-exit.txt").exists());

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_kills_background_descendants() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let command = concat!(
        "(sleep 1; printf leaked > leaked-after-cancel.txt) & ",
        "printf ready > ready.txt; ",
        "wait"
    )
    .to_owned();

    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();

    let task = tokio::spawn(async move {
        execute_command(
            &sandbox,
            &command,
            false,
            CommandApproval::confirmed_for(&command, false),
            ExecOptions::new(Duration::from_secs(10), 2048),
            worker_cancel,
        )
        .await
    });

    wait_until_exists(&root.path().join("ready.txt"), Duration::from_secs(3)).await?;

    cancel.cancel();

    let joined = tokio::time::timeout(Duration::from_secs(3), task).await?;

    let result = joined?;

    assert!(
        matches!(result, Err(ExecError::Cancelled { .. })),
        "unexpected descendant cancellation result: {result:?}"
    );

    tokio::time::sleep(Duration::from_millis(1_300)).await;

    assert!(!root.path().join("leaked-after-cancel.txt").exists());

    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn cancellation_kills_job_object_descendants() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let command = concat!(
        "echo ready>ready.txt & ",
        "ping.exe 127.0.0.1 -n 4 >NUL & ",
        "echo leaked>leaked-after-cancel.txt"
    )
    .to_owned();

    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();

    let task = tokio::spawn(async move {
        execute_command(
            &sandbox,
            &command,
            false,
            CommandApproval::confirmed_for(&command, false),
            ExecOptions::new(Duration::from_secs(15), 2048),
            worker_cancel,
        )
        .await
    });

    wait_until_exists(&root.path().join("ready.txt"), Duration::from_secs(5)).await?;

    cancel.cancel();

    let joined = tokio::time::timeout(Duration::from_secs(5), task).await?;

    let result = joined?;

    assert!(
        matches!(result, Err(ExecError::Cancelled { .. })),
        "unexpected Windows descendant cancellation result: {result:?}"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;

    assert!(!root.path().join("leaked-after-cancel.txt").exists());

    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shell_preserves_nested_quotes() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;
    let command = r#"for %A in ("quoted value") do @echo [%~A]"#;

    let output = execute_command(
        &sandbox,
        command,
        false,
        CommandApproval::confirmed_for(command, false),
        ExecOptions::new(Duration::from_secs(3), 1_024),
        CancellationToken::new(),
    )
    .await?;

    assert_eq!(output.trim(), "[quoted value]");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn stdin_is_closed() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let command = concat!(
        "if IFS= read -r line; ",
        "then printf unexpected; ",
        "else printf stdin-closed; fi"
    );

    let output = execute_command(
        &sandbox,
        command,
        false,
        CommandApproval::confirmed_for(command, false),
        ExecOptions::new(Duration::from_secs(2), 1024),
        CancellationToken::new(),
    )
    .await?;

    assert_eq!(output, "stdin-closed");
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn terminal_control_sequences_are_sanitized() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let sandbox = SandboxRoot::open(root.path())?;

    let command = r"printf '\033\000END\n'";

    let output = execute_command(
        &sandbox,
        command,
        false,
        CommandApproval::confirmed_for(command, false),
        ExecOptions::new(Duration::from_secs(2), 1024),
        CancellationToken::new(),
    )
    .await?;

    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\0'));
    assert!(output.contains("\\x1b"));
    assert!(output.contains("\\x00"));
    assert!(output.contains("END"));

    Ok(())
}

async fn wait_until_exists(path: &Path, timeout: Duration) -> Result<(), Box<dyn Error>> {
    tokio::time::timeout(timeout, async {
        loop {
            match tokio::fs::metadata(path).await {
                Ok(_) => return Ok::<(), io::Error>(()),
                Err(source) if source.kind() == io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(source) => return Err(source),
            }
        }
    })
    .await??;

    Ok(())
}

#[cfg(unix)]
fn long_output_command() -> String {
    concat!(
        "i=0; ",
        "while [ \"$i\" -lt 4000 ]; do ",
        "printf 0123456789; ",
        "i=$((i+1)); ",
        "done; ",
        "printf TAIL"
    )
    .to_owned()
}

#[cfg(windows)]
fn long_output_command() -> String {
    concat!(
        "for /L %i in (1,1,4000) do @echo 0123456789 & ",
        "echo TAIL"
    )
    .to_owned()
}

#[cfg(unix)]
fn slow_command() -> String {
    "sleep 5".to_owned()
}

#[cfg(windows)]
fn slow_command() -> String {
    "ping.exe 127.0.0.1 -n 6 >NUL".to_owned()
}

#[cfg(unix)]
fn marker_command(path: &str) -> String {
    format!("printf forbidden > {path}")
}

#[cfg(windows)]
fn marker_command(path: &str) -> String {
    format!("echo forbidden>{path}")
}

#[cfg(unix)]
fn configured_read_only_command() -> (&'static str, Vec<&'static str>, &'static str) {
    ("uname", vec!["-n"], "uname -n")
}

#[cfg(windows)]
fn configured_read_only_command() -> (&'static str, Vec<&'static str>, &'static str) {
    ("whoami", vec!["/user"], "whoami /user")
}

#[cfg(unix)]
fn stdin_echo_program() -> Result<(std::path::PathBuf, Vec<String>), Box<dyn Error>> {
    let path = std::path::PathBuf::from("/bin/cat");
    if !path.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "/bin/cat is unavailable").into());
    }
    Ok((path, Vec::new()))
}

#[cfg(windows)]
fn stdin_echo_program() -> Result<(std::path::PathBuf, Vec<String>), Box<dyn Error>> {
    let path = windows_system_program("findstr.exe")?;
    Ok((path, vec![".*".to_owned()]))
}

#[cfg(unix)]
fn slow_direct_program() -> Result<(std::path::PathBuf, Vec<String>), Box<dyn Error>> {
    let path = std::path::PathBuf::from("/bin/sleep");
    if !path.is_file() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "/bin/sleep is unavailable").into());
    }
    Ok((path, vec!["5".to_owned()]))
}

#[cfg(windows)]
fn slow_direct_program() -> Result<(std::path::PathBuf, Vec<String>), Box<dyn Error>> {
    let path = windows_system_program("ping.exe")?;
    Ok((
        path,
        vec!["-n".to_owned(), "6".to_owned(), "127.0.0.1".to_owned()],
    ))
}

#[cfg(unix)]
fn definitely_missing_absolute_program() -> std::path::PathBuf {
    std::path::PathBuf::from("/definitely/missing/decode-hook")
}

#[cfg(windows)]
fn definitely_missing_absolute_program() -> std::path::PathBuf {
    std::path::PathBuf::from(r"C:\definitely\missing\decode-hook.exe")
}

#[cfg(windows)]
fn windows_system_program(name: &str) -> Result<std::path::PathBuf, Box<dyn Error>> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SystemRoot is unavailable"))?;
    let path = std::path::PathBuf::from(system_root)
        .join("System32")
        .join(name);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("{} is unavailable", path.display()),
        )
        .into());
    }
    Ok(path)
}
