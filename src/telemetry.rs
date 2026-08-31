use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{EnvFilter, fmt, fmt::format::FmtSpan, prelude::*};

use crate::{config::AppConfig, error::AppError};

pub struct TelemetryGuard {
    _worker_guard: WorkerGuard,
}

pub fn init(config: &AppConfig) -> Result<TelemetryGuard, AppError> {
    std::fs::create_dir_all(&config.logging.dir)?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("decode.log")
        .build(&config.logging.dir)
        .map_err(|error| AppError::Telemetry(error.to_string()))?;
    let (non_blocking, worker_guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(_) => EnvFilter::try_new(&config.logging.level)
            .map_err(|error| AppError::Telemetry(error.to_string()))?,
    };

    let file_layer = fmt::layer()
        .json()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(file_layer)
        .try_init()
        .map_err(|error| AppError::Telemetry(error.to_string()))?;

    Ok(TelemetryGuard {
        _worker_guard: worker_guard,
    })
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        io::{self, Write},
        path::PathBuf,
        process::Command,
        sync::{Arc, Mutex},
    };

    use secrecy::SecretString;
    use tempfile::{TempDir, tempdir};

    use crate::config::{AppConfig, CliArgs};

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("telemetry test buffer lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn test_config(log_dir: PathBuf) -> Result<(TempDir, AppConfig), Box<dyn Error>> {
        let root = tempdir()?;
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace)?;
        let instructions = workspace.join("instructions.md");
        fs::write(&instructions, "system\n")?;
        let credentials = root.path().join("credentials.env");
        fs::write(&credentials, "AZURE_OPENAI_API_KEY=test-key\n")?;
        let config_file = root.path().join("config.toml");
        let skills = root.path().join("skills");
        fs::write(
            &config_file,
            format!(
                concat!(
                    "[api]\n",
                    "responses_url = 'https://example.test/v1/responses'\n",
                    "deployment = 'model'\n",
                    "[agent.skills]\n",
                    "user_dir = '{}'\n"
                ),
                skills.display()
            ),
        )?;
        let config = AppConfig::load_from(CliArgs {
            config_file: Some(config_file),
            env_file: Some(credentials),
            workspace: Some(workspace),
            instructions_file: Some(instructions),
            log_dir: Some(log_dir),
            ..CliArgs::default()
        })?;
        Ok((root, config))
    }

    fn enter_isolated_test(marker: &str, test_name: &str) -> Result<bool, Box<dyn Error>> {
        if std::env::var_os(marker).is_some() {
            return Ok(true);
        }
        let status = Command::new(std::env::current_exe()?)
            .args(["--exact", test_name, "--nocapture"])
            .env(marker, "1")
            .env_remove("RUST_LOG")
            .status()?;
        assert!(status.success(), "isolated telemetry test failed");
        Ok(false)
    }

    #[test]
    fn structured_json_keeps_metadata_and_redacts_secret_wrappers() -> Result<(), io::Error> {
        let output = Arc::new(Mutex::new(Vec::new()));
        let writer_output = Arc::clone(&output);
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_ansi(false)
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(move || SharedWriter(Arc::clone(&writer_output)))
            .finish();
        let azure = SecretString::from("azure-fake-secret-42");
        let aws = SecretString::from("aws-fake-session-token-84");
        let mcp = SecretString::from("mcp-fake-bearer-21");
        let prompt = "private prompt fixture";
        let body = "private request body fixture";
        let headers = "Authorization: private header fixture";

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "provider.request",
                provider = "azure",
                model = "gpt-fixture",
                session_id = "session-fixture",
                azure = ?azure,
                aws = ?aws,
                mcp = ?mcp
            );
            let _entered = span.enter();
            tracing::info!(turn_id = 7, attempt = 1, status = "ok", "request finished");
        });

        let bytes = output
            .lock()
            .map_err(|_| io::Error::other("telemetry test buffer lock poisoned"))?;
        let json = String::from_utf8_lossy(&bytes);
        for expected in [
            "provider.request",
            "azure",
            "gpt-fixture",
            "session-fixture",
            "ok",
        ] {
            assert!(
                json.contains(expected),
                "missing structured field {expected:?}"
            );
        }
        for forbidden in [
            "azure-fake-secret-42",
            "aws-fake-session-token-84",
            "mcp-fake-bearer-21",
            prompt,
            body,
            headers,
        ] {
            assert!(!json.contains(forbidden), "telemetry leaked {forbidden:?}");
        }
        Ok(())
    }

    #[test]
    fn instrumented_sensitive_boundaries_skip_all_arguments() -> Result<(), io::Error> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for relative in [
            "src/api/client.rs",
            "src/mcp/client.rs",
            "src/tools/mod.rs",
            "src/agent/orchestrator.rs",
            "src/agent/subagents.rs",
        ] {
            let source = std::fs::read_to_string(root.join(relative))?;
            let instruments = source.matches("#[tracing::instrument(").count();
            let skipped = source.matches("skip_all,").count();
            assert_eq!(
                instruments, skipped,
                "{relative} has an instrumented boundary that may capture arguments"
            );
        }
        Ok(())
    }

    #[test]
    fn tracing_macros_do_not_record_payload_or_process_output_fields() -> Result<(), io::Error> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for relative in [
            "src/api/client.rs",
            "src/mcp/client.rs",
            "src/lsp/client.rs",
            "src/agent/orchestrator.rs",
        ] {
            let source = std::fs::read_to_string(root.join(relative))?;
            for macro_name in [
                "tracing::trace!(",
                "tracing::debug!(",
                "tracing::info!(",
                "tracing::warn!(",
                "tracing::error!(",
            ] {
                for (start, _) in source.match_indices(macro_name) {
                    let tail = &source[start..];
                    let block = tail
                        .find(");")
                        .map_or(tail, |end| &tail[..end.saturating_add(2)]);
                    for forbidden in [
                        "prompt =",
                        "body =",
                        "header =",
                        "headers =",
                        "content =",
                        "stderr =",
                        "note =",
                    ] {
                        assert!(
                            !block.contains(forbidden),
                            "{relative} records forbidden field {forbidden:?}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn duplicate_initialization_returns_an_error_instead_of_panicking() -> Result<(), Box<dyn Error>>
    {
        const MARKER: &str = "DECODE_TEST_TELEMETRY_DUPLICATE";
        if !enter_isolated_test(
            MARKER,
            "telemetry::tests::duplicate_initialization_returns_an_error_instead_of_panicking",
        )? {
            return Ok(());
        }

        let logs = tempdir()?;
        let (_root, config) = test_config(logs.path().to_path_buf())?;
        let guard = super::init(&config)?;
        let duplicate = super::init(&config);
        assert!(duplicate.is_err());
        drop(guard);
        Ok(())
    }

    #[test]
    fn invalid_configured_filter_is_rejected() -> Result<(), Box<dyn Error>> {
        const MARKER: &str = "DECODE_TEST_TELEMETRY_FILTER";
        if !enter_isolated_test(
            MARKER,
            "telemetry::tests::invalid_configured_filter_is_rejected",
        )? {
            return Ok(());
        }

        let logs = tempdir()?;
        let (_root, mut config) = test_config(logs.path().to_path_buf())?;
        config.logging.level = "decode[=info".to_owned();
        assert!(super::init(&config).is_err());
        Ok(())
    }

    #[test]
    fn log_file_creation_failure_is_returned_instead_of_panicking() -> Result<(), Box<dyn Error>> {
        const MARKER: &str = "DECODE_TEST_TELEMETRY_APPENDER";
        if !enter_isolated_test(
            MARKER,
            "telemetry::tests::log_file_creation_failure_is_returned_instead_of_panicking",
        )? {
            return Ok(());
        }

        let logs = tempdir()?;
        for offset in -1..=1 {
            let date = chrono::Utc::now() + chrono::Duration::days(offset);
            fs::create_dir(
                logs.path()
                    .join(format!("decode.log.{}", date.format("%Y-%m-%d"))),
            )?;
        }
        let (_root, config) = test_config(logs.path().to_path_buf())?;
        assert!(super::init(&config).is_err());
        Ok(())
    }
}
