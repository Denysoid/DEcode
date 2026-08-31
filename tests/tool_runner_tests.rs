use std::{error::Error, time::Duration};

use decode::{
    parser::{ToolAction, ToolOutcome},
    tools::{
        ApprovalBinding, ApprovalNonce, CommandApproval, CommandDigest, ExecOptions,
        MAX_WRITE_FILE_BYTES, ShellConfirmationMode, StrictAllowlistEntry, ToolRunner,
    },
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

#[test]
fn confirmation_policy_is_based_on_action_kind() {
    let file_action = ToolAction::ReadFile {
        path: "file.txt".to_owned(),
    };
    let command_action = ToolAction::ExecuteCommand {
        command: "echo safe-looking".to_owned(),
        requires_confirmation: false,
    };

    assert!(!ToolRunner::requires_confirmation(&file_action));
    assert!(ToolRunner::requires_confirmation(&command_action));
}

#[test]
fn configured_runner_exposes_strict_allowlist_confirmation_policy() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let options =
        ExecOptions::default().with_confirmation_mode(ShellConfirmationMode::StrictAllowlist);
    let runner = ToolRunner::with_exec_options(root.path(), options)?;
    let allowed = ToolAction::ExecuteCommand {
        command: "whoami".to_owned(),
        requires_confirmation: false,
    };
    let denied = ToolAction::ExecuteCommand {
        command: "cargo check".to_owned(),
        requires_confirmation: false,
    };

    assert!(!runner.action_requires_confirmation(&allowed));
    assert!(runner.action_requires_confirmation(&denied));
    Ok(())
}

#[test]
fn runner_uses_configured_exact_argv_for_confirmation_policy() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let (program, args, command) = configured_read_only_command();
    let options = ExecOptions::default()
        .with_confirmation_mode(ShellConfirmationMode::StrictAllowlist)
        .with_strict_allowlist_entries([StrictAllowlistEntry::new(program, args.iter().copied())?]);
    let runner = ToolRunner::with_exec_options(root.path(), options)?;
    let allowed = ToolAction::ExecuteCommand {
        command: command.to_owned(),
        requires_confirmation: false,
    };
    let different_argv = ToolAction::ExecuteCommand {
        command: format!("{command} extra"),
        requires_confirmation: false,
    };

    assert!(!runner.action_requires_confirmation(&allowed));
    assert!(runner.action_requires_confirmation(&different_argv));
    Ok(())
}

#[tokio::test]
async fn file_actions_run_without_approval() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let action = ToolAction::WriteFile {
        path: "nested/file.txt".to_owned(),
        content: "content".to_owned(),
    };

    let outcome = runner
        .execute_action(&action, None, CancellationToken::new())
        .await;

    assert!(matches!(outcome, ToolOutcome::Success(_)));
    assert_eq!(
        std::fs::read_to_string(root.path().join("nested/file.txt"))?,
        "content"
    );

    Ok(())
}

#[tokio::test]
async fn model_false_command_does_not_run_without_approval() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("must-not-exist.txt");
    let action = ToolAction::ExecuteCommand {
        command,
        requires_confirmation: false,
    };

    let outcome = runner
        .execute_action(&action, None, CancellationToken::new())
        .await;

    assert!(matches!(
        outcome,
        ToolOutcome::Failure { ref message }
            if message.contains("requires user confirmation")
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());

    Ok(())
}

#[tokio::test]
async fn exact_approval_runs_the_bound_command() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("created.txt");
    let action = ToolAction::ExecuteCommand {
        command: command.clone(),
        requires_confirmation: false,
    };

    let outcome = runner
        .execute_action(
            &action,
            Some(CommandApproval::confirmed_for(&command, false)),
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(outcome, ToolOutcome::Success(_)));
    assert!(root.path().join("created.txt").exists());

    Ok(())
}

#[tokio::test]
async fn approval_is_bound_to_model_flag_as_well_as_command() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("must-not-exist.txt");
    let action = ToolAction::ExecuteCommand {
        command: command.clone(),
        requires_confirmation: false,
    };

    let outcome = runner
        .execute_action(
            &action,
            Some(CommandApproval::confirmed_for(&command, true)),
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(
        outcome,
        ToolOutcome::Failure { ref message }
            if message.contains("does not match")
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());

    Ok(())
}

#[tokio::test]
async fn pre_cancelled_action_never_starts() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let action = ToolAction::WriteFile {
        path: "must-not-exist.txt".to_owned(),
        content: "content".to_owned(),
    };
    let cancel = CancellationToken::new();
    cancel.cancel();

    let outcome = runner.execute_action(&action, None, cancel).await;

    assert!(matches!(
        outcome,
        ToolOutcome::Failure { ref message }
            if message.contains("cancelled before it started")
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());

    Ok(())
}

#[tokio::test]
async fn pre_cancelled_file_tool_matrix_never_reads_or_mutates() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    std::fs::write(root.path().join("existing.txt"), "old needle")?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let actions = [
        ToolAction::ReadFile {
            path: "existing.txt".to_owned(),
        },
        ToolAction::ListDirectory {
            path: ".".to_owned(),
        },
        ToolAction::SearchCode {
            pattern: "needle".to_owned(),
            path: None,
        },
        ToolAction::WriteFile {
            path: "must-not-exist.txt".to_owned(),
            content: "new".to_owned(),
        },
        ToolAction::ApplyPatch {
            path: "existing.txt".to_owned(),
            search: "old".to_owned(),
            replace: "new".to_owned(),
        },
    ];

    for action in actions {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let outcome = runner.execute_action(&action, None, cancel).await;
        assert!(matches!(
            outcome,
            ToolOutcome::Failure { ref message }
                if message.contains("cancelled before it started")
        ));
    }

    assert_eq!(
        std::fs::read_to_string(root.path().join("existing.txt"))?,
        "old needle"
    );
    assert!(!root.path().join("must-not-exist.txt").exists());
    Ok(())
}

#[tokio::test]
async fn bound_approval_nonce_is_consumed_once() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("bound-created.txt");
    let action = ToolAction::ExecuteCommand {
        command: command.clone(),
        requires_confirmation: false,
    };
    let binding = ApprovalBinding {
        epoch: 7,
        turn_id: 11,
        action_id: 13,
        nonce: ApprovalNonce::new([19; 16]),
        command_digest: CommandDigest::for_command(&command),
    };

    let first = runner
        .execute_action_bound(
            &action,
            Some(CommandApproval::confirmed_for_bound(
                &command, false, binding,
            )),
            binding,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(first, ToolOutcome::Success(_)));

    let replay = runner
        .execute_action_bound(
            &action,
            Some(CommandApproval::confirmed_for_bound(
                &command, false, binding,
            )),
            binding,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        replay,
        ToolOutcome::Failure { ref message } if message.contains("already been consumed")
    ));

    let next_epoch = ApprovalBinding {
        epoch: 8,
        action_id: 14,
        ..binding
    };
    let advanced = runner
        .execute_action_bound(
            &action,
            Some(CommandApproval::confirmed_for_bound(
                &command, false, next_epoch,
            )),
            next_epoch,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(advanced, ToolOutcome::Success(_)));

    let stale = ApprovalBinding {
        nonce: ApprovalNonce::new([23; 16]),
        ..binding
    };
    let stale_outcome = runner
        .execute_action_bound(
            &action,
            Some(CommandApproval::confirmed_for_bound(&command, false, stale)),
            stale,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        stale_outcome,
        ToolOutcome::Failure { ref message } if message.contains("epoch 7 is stale")
    ));

    Ok(())
}

#[tokio::test]
async fn invalid_timeout_does_not_consume_a_bound_approval() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("runs-after-valid-timeout.txt");
    let action = ToolAction::ExecuteCommand {
        command: command.clone(),
        requires_confirmation: false,
    };
    let binding = ApprovalBinding {
        epoch: 9,
        turn_id: 10,
        action_id: 11,
        nonce: ApprovalNonce::new([12; 16]),
        command_digest: CommandDigest::for_command(&command),
    };
    let approval = || {
        Some(CommandApproval::confirmed_for_bound(
            &command, false, binding,
        ))
    };

    let invalid = runner
        .execute_action_bound_with_timeout(
            &action,
            approval(),
            binding,
            Duration::ZERO,
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(
        invalid,
        ToolOutcome::Failure { ref message } if message.contains("timeout must be greater than zero")
    ));

    let retry = runner
        .execute_action_bound(&action, approval(), binding, CancellationToken::new())
        .await;
    assert!(matches!(retry, ToolOutcome::Success(_)));
    assert!(root.path().join("runs-after-valid-timeout.txt").exists());
    Ok(())
}

#[tokio::test]
async fn command_digest_must_match_bound_action() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let command = marker_command("must-not-exist.txt");
    let action = ToolAction::ExecuteCommand {
        command: command.clone(),
        requires_confirmation: false,
    };
    let binding = ApprovalBinding {
        epoch: 1,
        turn_id: 2,
        action_id: 3,
        nonce: ApprovalNonce::new([4; 16]),
        command_digest: CommandDigest::new([0; 32]),
    };

    let outcome = runner
        .execute_action_bound(
            &action,
            Some(CommandApproval::confirmed_for_bound(
                &command, false, binding,
            )),
            binding,
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(
        outcome,
        ToolOutcome::Failure { ref message } if message.contains("binding does not match")
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());
    Ok(())
}

#[tokio::test]
async fn oversized_write_is_rejected_before_mutation() -> Result<(), Box<dyn Error>> {
    let root = TempDir::new()?;
    let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
    let action = ToolAction::WriteFile {
        path: "must-not-exist.txt".to_owned(),
        content: "x".repeat(MAX_WRITE_FILE_BYTES + 1),
    };

    let outcome = runner
        .execute_action(&action, None, CancellationToken::new())
        .await;

    assert!(matches!(
        outcome,
        ToolOutcome::Failure { ref message } if message.contains("exceeding the permitted")
    ));
    assert!(!root.path().join("must-not-exist.txt").exists());
    Ok(())
}

#[cfg(unix)]
fn marker_command(path: &str) -> String {
    format!("printf created > {path}")
}

#[cfg(windows)]
fn marker_command(path: &str) -> String {
    format!("echo created>{path}")
}

#[cfg(unix)]
fn configured_read_only_command() -> (&'static str, Vec<&'static str>, &'static str) {
    ("uname", vec!["-n"], "uname -n")
}

#[cfg(windows)]
fn configured_read_only_command() -> (&'static str, Vec<&'static str>, &'static str) {
    ("whoami", vec!["/user"], "whoami /user")
}
