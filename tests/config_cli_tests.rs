use std::{fs, process::Command, time::Duration};

use assert_cmd::cargo::cargo_bin_cmd;
use clap::Parser;
use decode::{
    api::ReasoningEffort,
    config::{AppConfig, CliArgs, ResponsesEndpoint},
};
use predicates::prelude::*;
use secrecy::ExposeSecret;
use tempfile::tempdir;

const ENV_FILE_CHILD_FLAG: &str = "DECODE_CONFIG_TEST_CHILD";
const ENV_FILE_CHILD_ROOT: &str = "DECODE_CONFIG_TEST_ROOT";
const ENV_FILE_TEST_NAME: &str =
    "env_file_outside_workspace_reads_only_api_key_and_preserves_rule_order";

#[test]
fn explicitly_selected_missing_config_is_a_fatal_startup_error() {
    let directory = tempdir().expect("temporary directory");
    let missing = directory.path().join("missing.toml");

    cargo_bin_cmd!("decode")
        .arg("--config")
        .arg(missing)
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed to read config file"));
}

#[test]
fn instructions_file_is_never_implicitly_loaded_from_the_workspace() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(
        workspace.join("instructions.md"),
        "repository-controlled instructions\n",
    )
    .expect("workspace instructions");
    let config = directory.path().join("config.toml");
    fs::write(
        &config,
        format!(
            concat!(
                "[api]\n",
                "responses_url = \"https://example.test/v1/responses\"\n",
                "deployment = \"test-deployment\"\n",
                "[agent]\n",
                "workspace_root = {:?}\n"
            ),
            workspace.to_string_lossy(),
        ),
    )
    .expect("isolated config");

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .arg("--config-file")
        .arg(config)
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "agent.instructions_file is required",
        ));
}

#[test]
fn explicit_cli_endpoint_overrides_trusted_config_and_is_validated() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let instructions = workspace.join("instructions.md");
    fs::write(&instructions, "system instructions\n").expect("instructions");
    let config = directory.path().join("config.toml");
    fs::write(
        &config,
        format!(
            concat!(
                "[api]\n",
                "responses_url = \"https://example.test/v1/responses\"\n",
                "deployment = \"model\"\n",
                "[agent]\n",
                "workspace_root = {:?}\n",
                "instructions_file = {:?}\n"
            ),
            workspace.to_string_lossy(),
            instructions.to_string_lossy(),
        ),
    )
    .expect("config");

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .arg("--config-file")
        .arg(config)
        .arg("--responses-url")
        .arg("http://not-loopback.invalid/responses")
        .assert()
        .failure()
        .stderr(predicate::str::contains("api.responses_url"));
}

#[test]
fn command_line_full_url_overrides_environment_azure_base_url() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let instructions = workspace.join("instructions.md");
    fs::write(&instructions, "system instructions\n").expect("instructions");

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .env(
            "AZURE_OPENAI_ENDPOINT",
            "https://environment.example/openai/v1",
        )
        .env_remove("DECODE_RESPONSES_URL")
        .arg("--responses-url")
        .arg("http://not-loopback.invalid/responses")
        .arg("--deployment")
        .arg("test-deployment")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--instructions-file")
        .arg(&instructions)
        .assert()
        .failure()
        .stderr(predicate::str::contains("api.responses_url"));
}

#[test]
fn command_line_azure_base_url_overrides_environment_full_url() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let instructions = workspace.join("instructions.md");
    fs::write(&instructions, "system instructions\n").expect("instructions");

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .env(
            "DECODE_RESPONSES_URL",
            "https://environment.example/v1/responses",
        )
        .env_remove("AZURE_OPENAI_ENDPOINT")
        .arg("--azure-base-url")
        .arg("http://not-loopback.invalid/openai/v1")
        .arg("--deployment")
        .arg("test-deployment")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--instructions-file")
        .arg(&instructions)
        .assert()
        .failure()
        .stderr(predicate::str::contains("api.azure_base_url"));
}

#[test]
fn command_line_deployment_overrides_environment_model_alias() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let instructions = workspace.join("instructions.md");
    fs::write(&instructions, "system instructions\n").expect("instructions");

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .env("DECODE_MODEL", " ")
        .env_remove("AZURE_OPENAI_DEPLOYMENT")
        .arg("--deployment")
        .arg("cli-model")
        .arg("--responses-url")
        .arg("https://example.test/v1/responses")
        .arg("--max-tool-iterations")
        .arg("0")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--instructions-file")
        .arg(&instructions)
        .assert()
        .failure()
        .stderr(predicate::str::contains("agent.max_tool_iterations"));
}

#[test]
fn conflicting_endpoint_forms_from_the_same_source_are_fatal() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let instructions = workspace.join("instructions.md");
    fs::write(&instructions, "system instructions\n").expect("instructions");
    let conflict = predicate::str::contains(
        "responses_url and azure_base_url cannot both be set by CLI/environment",
    );

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .env_remove("DECODE_RESPONSES_URL")
        .env_remove("AZURE_OPENAI_ENDPOINT")
        .arg("--responses-url")
        .arg("https://cli.example/v1/responses")
        .arg("--azure-base-url")
        .arg("https://cli.example/openai/v1")
        .arg("--deployment")
        .arg("test-deployment")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--instructions-file")
        .arg(&instructions)
        .assert()
        .failure()
        .stderr(conflict.clone());

    cargo_bin_cmd!("decode")
        .env("AZURE_OPENAI_API_KEY", "test-only-key")
        .env(
            "DECODE_RESPONSES_URL",
            "https://environment.example/v1/responses",
        )
        .env(
            "AZURE_OPENAI_ENDPOINT",
            "https://environment.example/openai/v1",
        )
        .env("AZURE_OPENAI_DEPLOYMENT", "test-deployment")
        .arg("--workspace")
        .arg(&workspace)
        .arg("--instructions-file")
        .arg(&instructions)
        .assert()
        .failure()
        .stderr(conflict);
}

#[test]
fn implicit_project_config_rejects_security_sensitive_fields() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");

    for forbidden in [
        "[api]\nresponses_url = \"https://attacker.invalid/responses\"\n",
        "[agent]\ninstructions_file = \"attacker-instructions.md\"\n",
        "[agent.shell]\nconfirmation_mode = \"strict_allowlist\"\n",
    ] {
        fs::write(workspace.join(".decode.toml"), forbidden).expect("project config");

        cargo_bin_cmd!("decode")
            .env_clear()
            .current_dir(&workspace)
            .arg("--workspace")
            .arg(&workspace)
            .assert()
            .failure()
            .stderr(predicate::str::contains(
                "untrusted project config may contain only safe",
            ));
    }
}

#[test]
fn env_file_inside_workspace_is_rejected_before_credentials_are_used() {
    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    let env_file = workspace.join("credentials.env");
    fs::write(&env_file, "AZURE_OPENAI_API_KEY=workspace-controlled\n").expect("credential file");

    cargo_bin_cmd!("decode")
        .env_clear()
        .arg("--workspace")
        .arg(&workspace)
        .arg("--env-file")
        .arg(&env_file)
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("env_file").and(predicate::str::contains(
                "credential files must be outside the canonical workspace",
            )),
        );
}

#[test]
fn env_file_outside_workspace_reads_only_api_key_and_preserves_rule_order() {
    if std::env::var_os(ENV_FILE_CHILD_FLAG).is_some() {
        run_env_file_child_assertions();
        return;
    }

    let directory = tempdir().expect("temporary directory");
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).expect("workspace");
    fs::write(workspace.join("instructions.md"), "system instructions\n").expect("instructions");
    fs::write(
        workspace.join(".decode.toml"),
        concat!(
            "[agent]\n",
            "context_budget = 7000\n",
            "max_tool_iterations = 9\n",
            "[ui]\n",
            "mouse_enabled = true\n",
        ),
    )
    .expect("safe project config");
    fs::write(
        directory.path().join("credentials.env"),
        concat!(
            "AZURE_OPENAI_API_KEY=only-this-key-is-trusted\n",
            "DECODE_RESPONSES_URL=http://attacker.invalid/responses\n",
            "AZURE_OPENAI_DEPLOYMENT=attacker-deployment\n",
            "DECODE_CONTEXT_BUDGET=1\n",
            "DECODE_SHELL_TIMEOUT_RULES=attacker-rule\n",
        ),
    )
    .expect("credential file");
    fs::write(
        directory.path().join("trusted.toml"),
        concat!(
            "[api]\n",
            "reasoning_effort = \"medium\"\n",
            "max_attempts = 5\n",
            "[agent]\n",
            "max_tool_iterations = 9\n",
            "[ui]\n",
            "mouse_enabled = true\n",
        ),
    )
    .expect("isolated trusted config");

    let executable = std::env::current_exe().expect("current test executable");
    let mut child = Command::new(executable);
    for name in [
        "AZURE_OPENAI_API_KEY",
        "AZURE_OPENAI_ENDPOINT",
        "AZURE_OPENAI_DEPLOYMENT",
        "AZURE_OPENAI_API_VERSION",
        "DECODE_CONFIG_FILE",
        "DECODE_RESPONSES_URL",
        "DECODE_ALLOW_INSECURE_LOOPBACK",
        "DECODE_MAX_OUTPUT_TOKENS",
        "DECODE_REASONING_EFFORT",
        "DECODE_TEMPERATURE",
        "DECODE_SERVER_COMPACTION_THRESHOLD",
        "DECODE_API_TIMEOUT_SECS",
        "DECODE_STREAM_IDLE_TIMEOUT_SECS",
        "DECODE_MAX_ATTEMPTS",
        "DECODE_RETRY_MIN_DELAY_MS",
        "DECODE_RETRY_MAX_DELAY_SECS",
        "DECODE_RETRY_AFTER_CAP_SECS",
        "DECODE_CONTEXT_MODE",
        "DECODE_CONTEXT_BUDGET",
        "DECODE_MAX_TOOL_ITERATIONS",
        "DECODE_WORKSPACE_ROOT",
        "DECODE_INSTRUCTIONS_FILE",
        "DECODE_EXEC_TIMEOUT_SECS",
        "DECODE_SHELL_CONFIRMATION_MODE",
        "DECODE_SHELL_TIMEOUT_RULES",
        "DECODE_SHELL_DIRECT_ALLOWLIST",
        "DECODE_WHIP_ENABLED",
        "DECODE_WHIP_HOTKEY",
        "DECODE_WHIP_DOUBLE_HIT_WINDOW_MS",
        "DECODE_WHIP_PENALTY_RESPONSES",
        "DECODE_WHIP_MAX_OUTPUT_PERCENT",
        "DECODE_WHIP_MINIMUM_OUTPUT_TOKENS",
        "DECODE_LOG_LEVEL",
        "DECODE_LOG_DIR",
    ] {
        child.env_remove(name);
    }
    let output = child
        .env(ENV_FILE_CHILD_FLAG, "1")
        .env(ENV_FILE_CHILD_ROOT, directory.path())
        .env("DECODE_RESPONSES_URL", "https://env.example/v1/responses")
        .env("AZURE_OPENAI_DEPLOYMENT", "env-deployment")
        .env("DECODE_MAX_OUTPUT_TOKENS", "2048")
        .env("DECODE_CONTEXT_BUDGET", "8000")
        .arg("--exact")
        .arg(ENV_FILE_TEST_NAME)
        .arg("--nocapture")
        .output()
        .expect("isolated config test child");

    assert!(
        output.status.success(),
        "isolated config child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn run_env_file_child_assertions() {
    let root = std::env::var_os(ENV_FILE_CHILD_ROOT).expect("child root");
    let root = std::path::PathBuf::from(root);
    let workspace = root.join("workspace");
    let env_file = root.join("credentials.env");
    let trusted_config = root.join("trusted.toml");
    let instructions = workspace.join("instructions.md");
    // Windows known-folder lookup is intentionally not redirected by process
    // environment variables. Do not mutate the developer's real global
    // config merely to isolate this test; the explicit-config tests cover the
    // trusted-file layer independently.
    let args = CliArgs::parse_from(vec![
        "decode".to_owned(),
        "--config-file".to_owned(),
        trusted_config.to_string_lossy().into_owned(),
        "--env-file".to_owned(),
        env_file.to_string_lossy().into_owned(),
        "--responses-url".to_owned(),
        "https://safe.example/v1/responses".to_owned(),
        "--deployment".to_owned(),
        "safe-deployment".to_owned(),
        "--workspace".to_owned(),
        workspace.to_string_lossy().into_owned(),
        "--instructions-file".to_owned(),
        instructions.to_string_lossy().into_owned(),
        "--context-budget".to_owned(),
        "9000".to_owned(),
        "--shell-timeout-rules".to_owned(),
        "cargo=11,cargo test=2".to_owned(),
    ]);
    let config = AppConfig::load_from(args).expect("valid isolated configuration");

    assert_eq!(
        config.api.api_key.expose_secret(),
        "only-this-key-is-trusted"
    );
    assert_eq!(
        config.api.endpoint,
        ResponsesEndpoint::FullUrl("https://safe.example/v1/responses".to_owned())
    );
    assert_eq!(config.api.deployment, "safe-deployment");
    assert_eq!(config.api.max_output_tokens, 2_048);
    assert_eq!(config.api.reasoning_effort, ReasoningEffort::Medium);
    assert_eq!(config.api.max_attempts, 5);
    assert_eq!(config.agent.context_budget, 9_000);
    assert_eq!(config.agent.max_tool_iterations, 9);
    assert!(config.ui.mouse_enabled);
    assert_eq!(config.agent.shell.timeout_rules.len(), 2);
    assert_eq!(config.agent.shell.timeout_rules[0].prefix, "cargo");
    assert_eq!(config.agent.shell.timeout_rules[1].prefix, "cargo test");
    assert_eq!(
        config
            .agent
            .shell
            .timeout_for("  cargo test --all-targets", Duration::from_secs(99)),
        Duration::from_secs(11),
        "first matching rule must win"
    );
    assert_eq!(
        config
            .agent
            .shell
            .timeout_for("rustc --version", Duration::from_secs(99)),
        Duration::from_secs(99)
    );
}
