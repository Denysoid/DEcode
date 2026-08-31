use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("user requested exit")]
    UserExit,
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),
    #[error("API error: {0}")]
    Api(#[from] ApiError),
    #[error("attachment error: {0}")]
    Attachment(#[from] crate::attachments::AttachmentError),
    #[error("onboarding error: {0}")]
    Onboarding(#[from] crate::onboarding::OnboardingError),
    #[error("tool error: {0}")]
    Tool(#[from] crate::tools::ToolError),
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] crate::agent::checkpoint::CheckpointError),
    #[error("session persistence error: {0}")]
    Session(#[from] crate::agent::persistence::SessionError),
    #[error("MCP error: {0}")]
    Mcp(#[from] crate::mcp::McpError),
    #[error("LSP error: {0}")]
    Lsp(#[from] crate::lsp::LspError),
    #[error("privacy shield error: {0}")]
    Privacy(#[from] crate::privacy::PrivacyError),
    #[error("plugin error: {0}")]
    Plugin(#[from] crate::plugins::PluginError),
    #[error("GitHub integration error: {0}")]
    GitHub(#[from] crate::github::GitHubError),
    #[error("sub-agent error: {0}")]
    Subagent(#[from] crate::agent::subagents::SubagentError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal error: {0}")]
    Terminal(String),
    #[error("telemetry initialization error: {0}")]
    Telemetry(String),
    #[error("orchestrator task failed: {0}")]
    OrchestratorTask(String),
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error(
        "provider API key is not set (AZURE_OPENAI_API_KEY, OPENAI_API_KEY, or DECODE_PROVIDER_API_KEY)"
    )]
    MissingApiKey,
    #[error(
        "api.responses_url or api.azure_base_url is required (or set DECODE_RESPONSES_URL/AZURE_OPENAI_ENDPOINT)"
    )]
    MissingResponsesUrl,
    #[error("api deployment/model is required (set AZURE_OPENAI_DEPLOYMENT or DECODE_MODEL)")]
    MissingDeployment,
    #[error(
        "agent.instructions_file is required; provide an explicit absolute path via --instructions-file, DECODE_INSTRUCTIONS_FILE, or a trusted config"
    )]
    MissingInstructionsFile,
    #[error("credential store error: {0}")]
    CredentialStore(String),
    #[error("invalid {field}: {message}")]
    InvalidValue {
        field: &'static str,
        message: String,
    },
    #[error("failed to determine the current directory: {0}")]
    CurrentDirectory(#[source] std::io::Error),
    #[error("failed to read config file {path}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("failed to resolve {field} path {path}: {source}")]
    PathIo {
        field: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{field} is not a directory: {path}")]
    NotDirectory { field: &'static str, path: PathBuf },
    #[error("system instructions path is not a regular file: {0}")]
    InstructionsNotFile(PathBuf),
    #[error("system instructions path must not be a symbolic link or reparse point: {0}")]
    InstructionsSymlink(PathBuf),
    #[error("failed to read system instructions from {path}: {source}")]
    InstructionsIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("invalid Responses URL: {0}")]
    InvalidUrl(String),
    #[error("invalid HTTP header: {0}")]
    InvalidHeader(String),
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("AWS Bedrock Runtime {stage} error: {message}")]
    Bedrock {
        stage: String,
        message: String,
        retryable: bool,
    },
    #[error("Responses WebSocket {stage} error: {message}")]
    WebSocket {
        stage: String,
        message: String,
        retryable: bool,
    },
    #[error("request timed out after {secs}s")]
    RequestTimeout { secs: u64 },
    #[error("HTTP {status}: {body}")]
    Http {
        status: u16,
        body: String,
        /// Parsed and capped Retry-After delay, when the server supplied one.
        retry_after_secs: Option<u64>,
    },
    #[error("rate limited; retry after {retry_after_secs}s: {body}")]
    RateLimited { retry_after_secs: u64, body: String },
    #[error("expected Content-Type text/event-stream, got {found}")]
    InvalidContentType { found: String },
    #[error("Responses protocol error: {0}")]
    Protocol(String),
    #[error("remote Responses error{code}: {message}")]
    Remote { code: String, message: String },
    #[error("stream idle timeout after {secs}s")]
    IdleTimeout { secs: u64 },
    #[error("request cancelled")]
    Cancelled,
    #[error("retry limit reached after {attempts} attempts: {last_error}")]
    RetryExhausted { attempts: u32, last_error: String },
}

impl ApiError {
    pub fn remote(code: Option<&str>, message: impl Into<String>) -> Self {
        Self::Remote {
            code: code
                .filter(|value| !value.is_empty())
                .map(|value| format!(" ({value})"))
                .unwrap_or_default(),
            message: message.into(),
        }
    }
}
