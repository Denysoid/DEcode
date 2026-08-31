use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

use crate::{
    api::types::{ContextManagement, ReasoningEffort},
    code_index::CodeIndexConfig,
    error::ConfigError,
    lsp::{LspConfig, LspServerConfig},
    mcp::{
        McpApprovalMode, McpConfig, McpOAuthConfig, McpPermissionConfig, McpServerConfig,
        McpTransportConfig,
    },
    tools::{ShellConfirmationMode, StrictAllowlistEntry},
    usage::{DeploymentPricing, PricingCatalog, PricingSource},
};

const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
const DEFAULT_CONTEXT_BUDGET: u32 = 120_000;
pub const MAX_CONTEXT_BUDGET: u32 = 2_000_000;
const DEFAULT_MAX_TOOL_ITERATIONS: u32 = 20;
const MAX_TOOL_ITERATIONS: u32 = 100;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;
// Reasoning deployments can legitimately stay silent for well over 30 seconds
// between SSE frames, especially at xhigh/max effort.  Keep this distinct from
// the header/request timeout and from the finite retry budget: the former
// detects a genuinely silent body, while the latter still prevents an endless
// Azure reconnect loop.
const DEFAULT_STREAM_IDLE_TIMEOUT_SECS: u64 = 180;
const DEFAULT_EXEC_TIMEOUT_SECS: u64 = 120;
const DEFAULT_SUBAGENT_TASK_TIMEOUT_SECS: u64 = 30 * 60;
const DEFAULT_SUBAGENT_GIT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_SUBAGENT_MAX_PARALLEL: u16 = 4;
const DEFAULT_SUBAGENT_MAX_PER_SESSION: u16 = 16;
const DEFAULT_SUBAGENT_MAX_TOOL_ITERATIONS: u32 = 12;
const DEFAULT_SUBAGENT_MAX_DEPTH: u8 = 3;
const DEFAULT_SUBAGENT_MAX_CHILDREN: u16 = 4;
const DEFAULT_SUBAGENT_MAX_TOKENS_PER_AGENT: u64 = 150_000;
const DEFAULT_SUBAGENT_MAX_TOTAL_TOKENS_PER_SESSION: u64 = 500_000;
const MAX_SUBAGENT_TOKEN_BUDGET: u64 = 100_000_000;
const DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCE_BYTES: usize = 64 * 1024;
const DEFAULT_PROJECT_INSTRUCTION_MAX_TOTAL_BYTES: usize = 256 * 1024;
const DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCES: usize = 64;
const DEFAULT_PROJECT_INSTRUCTION_MAX_INCLUDE_DEPTH: usize = 4;
const DEFAULT_SKILLS_METADATA_BUDGET_BYTES: usize = 16 * 1024;
const DEFAULT_SKILLS_MAX_SKILLS: usize = 128;
const DEFAULT_SKILLS_MAX_SKILL_BYTES: usize = 128 * 1024;
const DEFAULT_SKILLS_MAX_RESOURCE_BYTES: usize = 256 * 1024;
const DEFAULT_SKILLS_MAX_RESOURCES: usize = 256;
const DEFAULT_MAX_ATTEMPTS: u32 = 5;
const DEFAULT_RETRY_MIN_DELAY_MS: u64 = 500;
const DEFAULT_RETRY_MAX_DELAY_SECS: u64 = 30;
const MAX_RETRY_AFTER_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const MAX_INSTRUCTIONS_BYTES: u64 = 1024 * 1024;
const MAX_PROJECT_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_PROJECT_INSTRUCTION_TOTAL_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROJECT_INSTRUCTION_SOURCES: usize = 256;
const MAX_PROJECT_INSTRUCTION_INCLUDE_DEPTH: usize = 16;
const MAX_SKILLS_METADATA_BUDGET_BYTES: usize = 256 * 1024;
const MAX_SKILLS: usize = 512;
const MAX_SKILL_BYTES: usize = 1024 * 1024;
const MAX_SKILL_RESOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_SKILL_RESOURCES: usize = 2_048;
const DEFAULT_TERMINAL_MAX_SESSIONS: usize = 6;
const DEFAULT_TERMINAL_SCROLLBACK_LINES: usize = 10_000;
const MAX_TERMINAL_SESSIONS: usize = 16;
const MAX_TERMINAL_SCROLLBACK_LINES: usize = 100_000;
const MAX_TERMINAL_ARGUMENTS: usize = 64;
const MAX_TERMINAL_ARGUMENT_BYTES: usize = 4_096;

#[derive(Parser, Debug, Default)]
#[command(name = "decode", about = "DEcode by denysoid — AI coding agent")]
pub struct CliArgs {
    #[arg(
        long = "config",
        visible_alias = "config-file",
        short = 'c',
        env = "DECODE_CONFIG_FILE"
    )]
    pub config_file: Option<PathBuf>,
    /// Explicit credential file. It is never discovered implicitly, and only
    /// a supported key for the selected provider is read from it.
    #[arg(long)]
    pub env_file: Option<PathBuf>,

    /// Responses-compatible provider. Azure remains the fail-safe default.
    #[arg(long, env = "DECODE_PROVIDER")]
    pub provider: Option<String>,
    #[arg(long, env = "DECODE_PROVIDER_AUTH")]
    pub provider_auth: Option<String>,
    #[arg(long, env = "AWS_REGION")]
    pub aws_region: Option<String>,
    #[arg(long, env = "AWS_PROFILE")]
    pub aws_profile: Option<String>,
    #[arg(long, env = "DECODE_AWS_ROLE_ARN")]
    pub aws_role_arn: Option<String>,
    #[arg(long, env = "DECODE_BEDROCK_ENDPOINT_URL")]
    pub bedrock_endpoint_url: Option<String>,
    #[arg(long = "api-transport", env = "DECODE_API_TRANSPORT")]
    pub api_transport: Option<String>,

    #[arg(long, env = "DECODE_RESPONSES_URL")]
    pub responses_url: Option<String>,
    #[arg(long, env = "AZURE_OPENAI_ENDPOINT")]
    pub azure_base_url: Option<String>,
    #[arg(long, env = "DECODE_ALLOW_INSECURE_LOOPBACK")]
    pub allow_insecure_loopback: Option<bool>,
    #[arg(long, env = "AZURE_OPENAI_DEPLOYMENT")]
    pub deployment: Option<String>,
    #[arg(long = "model", env = "DECODE_MODEL")]
    pub model: Option<String>,
    #[arg(long, env = "DECODE_DEPLOYMENT_CHOICES", value_delimiter = ',')]
    pub deployment_choices: Option<Vec<String>>,
    #[arg(long, env = "AZURE_OPENAI_API_VERSION")]
    pub api_version: Option<String>,
    #[arg(long, env = "DECODE_MAX_OUTPUT_TOKENS")]
    pub max_output_tokens: Option<u32>,
    #[arg(long, env = "DECODE_REASONING_EFFORT")]
    pub reasoning_effort: Option<String>,
    #[arg(long, env = "DECODE_TEMPERATURE")]
    pub temperature: Option<f32>,
    #[arg(long, env = "DECODE_SERVER_COMPACTION_THRESHOLD")]
    pub server_compaction_threshold: Option<u64>,
    #[arg(long, env = "DECODE_API_TIMEOUT_SECS")]
    pub request_timeout_secs: Option<u64>,
    #[arg(long, env = "DECODE_STREAM_IDLE_TIMEOUT_SECS")]
    pub stream_idle_timeout_secs: Option<u64>,
    #[arg(long, env = "DECODE_MAX_ATTEMPTS")]
    pub max_attempts: Option<u32>,
    #[arg(long, env = "DECODE_RETRY_MIN_DELAY_MS")]
    pub retry_min_delay_ms: Option<u64>,
    #[arg(long, env = "DECODE_RETRY_MAX_DELAY_SECS")]
    pub retry_max_delay_secs: Option<u64>,
    #[arg(long, env = "DECODE_RETRY_AFTER_CAP_SECS")]
    pub retry_after_cap_secs: Option<u64>,

    #[arg(long, env = "DECODE_CONTEXT_MODE")]
    pub context_mode: Option<String>,
    #[arg(long, env = "DECODE_CONTEXT_BUDGET")]
    pub context_budget: Option<u32>,
    /// Trusted model capability ceiling used by the interactive context picker.
    #[arg(long, env = "DECODE_MAX_CONTEXT_BUDGET")]
    pub max_context_budget: Option<u32>,
    #[arg(long, env = "DECODE_MAX_TOOL_ITERATIONS")]
    pub max_tool_iterations: Option<u32>,
    #[arg(long, short = 'w', env = "DECODE_WORKSPACE_ROOT")]
    pub workspace: Option<PathBuf>,
    #[arg(long, env = "DECODE_SESSION_DIR")]
    pub session_dir: Option<PathBuf>,
    #[arg(long, env = "DECODE_INSTRUCTIONS_FILE")]
    pub instructions_file: Option<PathBuf>,
    #[arg(long, env = "DECODE_EXEC_TIMEOUT_SECS")]
    pub exec_timeout_secs: Option<u64>,
    #[arg(long, env = "DECODE_SUBAGENTS_ENABLED")]
    pub subagents_enabled: Option<bool>,
    #[arg(long, env = "DECODE_SUBAGENT_WORKTREE_DIR")]
    pub subagent_worktree_dir: Option<PathBuf>,
    #[arg(long, env = "DECODE_SUBAGENT_MAX_PARALLEL")]
    pub subagent_max_parallel: Option<u16>,
    #[arg(long, env = "DECODE_SUBAGENT_MAX_PER_SESSION")]
    pub subagent_max_per_session: Option<u16>,
    #[arg(long, env = "DECODE_SUBAGENT_MAX_TOOL_ITERATIONS")]
    pub subagent_max_tool_iterations: Option<u32>,
    #[arg(long, env = "DECODE_SUBAGENT_MAX_TOKENS_PER_AGENT")]
    pub subagent_max_tokens_per_agent: Option<u64>,
    #[arg(long, env = "DECODE_SUBAGENT_MAX_TOTAL_TOKENS_PER_SESSION")]
    pub subagent_max_total_tokens_per_session: Option<u64>,
    #[arg(long, env = "DECODE_SUBAGENT_TASK_TIMEOUT_SECS")]
    pub subagent_task_timeout_secs: Option<u64>,
    #[arg(long, env = "DECODE_SUBAGENT_GIT_TIMEOUT_SECS")]
    pub subagent_git_timeout_secs: Option<u64>,
    #[arg(long, env = "DECODE_SHELL_CONFIRMATION_MODE")]
    pub shell_confirmation_mode: Option<String>,
    #[arg(long, env = "DECODE_SHELL_TIMEOUT_RULES", value_delimiter = ',')]
    pub shell_timeout_rules: Option<Vec<String>>,
    /// Exact direct-exec argv entries in PROGRAM|ARG1|ARG2 form. These are
    /// considered only in strict_allowlist mode and never pass through a shell.
    #[arg(long, env = "DECODE_SHELL_DIRECT_ALLOWLIST", value_delimiter = ',')]
    pub shell_direct_allowlist: Option<Vec<String>>,
    #[arg(long, env = "DECODE_WHIP_ENABLED")]
    pub whip_enabled: Option<bool>,
    #[arg(long, env = "DECODE_WHIP_HOTKEY")]
    pub whip_hotkey: Option<String>,
    #[arg(long, env = "DECODE_WHIP_DOUBLE_HIT_WINDOW_MS")]
    pub whip_double_hit_window_ms: Option<u64>,
    #[arg(long, env = "DECODE_WHIP_PENALTY_RESPONSES")]
    pub whip_penalty_completed_responses: Option<u32>,
    #[arg(long, env = "DECODE_WHIP_MAX_OUTPUT_PERCENT")]
    pub whip_max_output_percent: Option<u8>,
    #[arg(long, env = "DECODE_WHIP_MINIMUM_OUTPUT_TOKENS")]
    pub whip_minimum_output_tokens: Option<u32>,

    #[arg(long, env = "DECODE_LOG_LEVEL")]
    pub log_level: Option<String>,
    #[arg(long, env = "DECODE_LOG_DIR")]
    pub log_dir: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    api: Option<FileApiConfig>,
    agent: Option<FileAgentConfig>,
    ui: Option<FileUiConfig>,
    logging: Option<FileLoggingConfig>,
    mcp: Option<FileMcpConfig>,
    lsp: Option<FileLspConfig>,
    code_index: Option<FileCodeIndexConfig>,
    github: Option<FileGitHubConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileGitHubConfig {
    enabled: Option<bool>,
    program: Option<String>,
    timeout_secs: Option<u64>,
    max_pull_requests: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileCodeIndexConfig {
    enabled: Option<bool>,
    auto_refresh: Option<bool>,
    max_files: Option<usize>,
    max_file_bytes: Option<usize>,
    max_source_bytes: Option<usize>,
    max_chunks: Option<usize>,
    chunk_lines: Option<usize>,
    overlap_lines: Option<usize>,
    max_result_bytes: Option<usize>,
    embeddings: Option<FileEmbeddingConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileEmbeddingConfig {
    enabled: Option<bool>,
    endpoint: Option<String>,
    model: Option<String>,
    dimensions: Option<usize>,
    batch_size: Option<usize>,
    max_chunks: Option<usize>,
    max_input_bytes: Option<usize>,
    request_timeout_secs: Option<u64>,
    max_attempts: Option<u32>,
    hybrid_weight: Option<f32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileLspConfig {
    enabled: Option<bool>,
    startup_timeout_secs: Option<u64>,
    request_timeout_secs: Option<u64>,
    max_message_bytes: Option<usize>,
    max_result_bytes: Option<usize>,
    max_diagnostics: Option<usize>,
    #[serde(default)]
    servers: Vec<FileLspServerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLspServerConfig {
    name: String,
    enabled: Option<bool>,
    required: Option<bool>,
    auto_start: Option<bool>,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    language_id: String,
    #[serde(default)]
    extensions: Vec<String>,
    #[serde(default)]
    root_markers: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileMcpConfig {
    enabled: Option<bool>,
    startup_timeout_secs: Option<u64>,
    tool_timeout_secs: Option<u64>,
    max_result_bytes: Option<usize>,
    max_sse_event_bytes: Option<usize>,
    reconnect_max_attempts: Option<u32>,
    reconnect_base_delay_ms: Option<u64>,
    #[serde(default)]
    servers: Vec<FileMcpServerConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMcpServerConfig {
    name: String,
    enabled: Option<bool>,
    required: Option<bool>,
    transport: String,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_from: BTreeMap<String, String>,
    working_directory: Option<PathBuf>,
    url: Option<String>,
    bearer_token_env: Option<String>,
    #[serde(default)]
    headers_from: BTreeMap<String, String>,
    oauth: Option<FileMcpOAuthConfig>,
    approval: Option<String>,
    #[serde(default)]
    enabled_tools: Vec<String>,
    #[serde(default)]
    disabled_tools: Vec<String>,
    #[serde(default)]
    trusted_read_only_tools: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileMcpOAuthConfig {
    client_id: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    callback_port: Option<u16>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PluginMcpContribution {
    #[serde(default)]
    servers: Vec<FileMcpServerConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct PluginLspContribution {
    #[serde(default)]
    servers: Vec<FileLspServerConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileApiConfig {
    provider: Option<String>,
    provider_auth: Option<String>,
    transport: Option<String>,
    aws_region: Option<String>,
    aws_profile: Option<String>,
    aws_role_arn: Option<String>,
    bedrock_endpoint_url: Option<String>,
    keyring_account: Option<String>,
    responses_url: Option<String>,
    azure_base_url: Option<String>,
    allow_insecure_loopback: Option<bool>,
    deployment: Option<String>,
    model: Option<String>,
    deployment_choices: Option<Vec<String>>,
    api_version: Option<String>,
    max_output_tokens: Option<u32>,
    reasoning_effort: Option<String>,
    temperature: Option<f32>,
    server_compaction_threshold: Option<u64>,
    request_timeout_secs: Option<u64>,
    stream_idle_timeout_secs: Option<u64>,
    max_attempts: Option<u32>,
    retry_min_delay_ms: Option<u64>,
    retry_max_delay_secs: Option<u64>,
    retry_after_cap_secs: Option<u64>,
    auto_pricing: Option<bool>,
    pricing_catalog_url: Option<String>,
    #[serde(default)]
    pricing: Vec<FileDeploymentPricing>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDeploymentPricing {
    deployment: String,
    input_usd_per_million: f64,
    cached_input_usd_per_million: Option<f64>,
    output_usd_per_million: f64,
    long_context_threshold_tokens: Option<u64>,
    long_context_input_usd_per_million: Option<f64>,
    long_context_cached_input_usd_per_million: Option<f64>,
    long_context_output_usd_per_million: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileAgentConfig {
    context_mode: Option<String>,
    context_budget: Option<u32>,
    max_context_budget: Option<u32>,
    max_tool_iterations: Option<u32>,
    workspace_root: Option<PathBuf>,
    session_dir: Option<PathBuf>,
    instructions_file: Option<PathBuf>,
    project_instructions: Option<FileProjectInstructionsConfig>,
    skills: Option<FileSkillsConfig>,
    exec_timeout_secs: Option<u64>,
    subagents: Option<FileSubagentConfig>,
    shell: Option<FileShellConfig>,
    whip: Option<FileWhipConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileSkillsConfig {
    enabled: Option<bool>,
    project_enabled: Option<bool>,
    user_dir: Option<PathBuf>,
    metadata_budget_bytes: Option<usize>,
    max_skills: Option<usize>,
    max_skill_bytes: Option<usize>,
    max_resource_bytes: Option<usize>,
    max_resources: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileProjectInstructionsConfig {
    enabled: Option<bool>,
    max_source_bytes: Option<usize>,
    max_total_bytes: Option<usize>,
    max_sources: Option<usize>,
    max_include_depth: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileSubagentConfig {
    enabled: Option<bool>,
    allow_mcp: Option<bool>,
    worktree_dir: Option<PathBuf>,
    max_parallel: Option<u16>,
    max_per_session: Option<u16>,
    max_tool_iterations: Option<u32>,
    max_tokens_per_agent: Option<u64>,
    max_total_tokens_per_session: Option<u64>,
    max_depth: Option<u8>,
    max_children_per_agent: Option<u16>,
    task_timeout_secs: Option<u64>,
    git_timeout_secs: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileShellConfig {
    confirmation_mode: Option<String>,
    #[serde(default)]
    timeout_rules: Vec<FileShellTimeoutRule>,
    #[serde(default)]
    direct_exec_allowlist: Vec<FileStrictAllowlistEntry>,
    terminal: Option<FileInteractiveTerminalConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileInteractiveTerminalConfig {
    enabled: Option<bool>,
    program: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    max_sessions: Option<usize>,
    scrollback_lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShellTimeoutRule {
    prefix: String,
    timeout_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileStrictAllowlistEntry {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileWhipConfig {
    enabled: Option<bool>,
    hotkey: Option<String>,
    double_hit_window_ms: Option<u64>,
    penalty_completed_responses: Option<u32>,
    max_output_percent: Option<u8>,
    minimum_output_tokens: Option<u32>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileUiConfig {
    confirm_destructive: Option<bool>,
    mouse_enabled: Option<bool>,
    language: Option<String>,
    onboarding_completed: Option<bool>,
    mascot_enabled: Option<bool>,
    show_thinking: Option<bool>,
    show_tool_activity: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct FileLoggingConfig {
    level: Option<String>,
    dir: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProjectFileConfig {
    agent: Option<ProjectAgentConfig>,
    ui: Option<ProjectUiConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProjectAgentConfig {
    context_mode: Option<String>,
    context_budget: Option<u32>,
    max_tool_iterations: Option<u32>,
    whip: Option<FileWhipConfig>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProjectUiConfig {
    mouse_enabled: Option<bool>,
}

#[derive(Clone)]
pub struct AppConfig {
    pub api: ApiConfig,
    pub deployment_choices: Vec<String>,
    pub agent: AgentConfig,
    pub ui: UiConfig,
    pub logging: LoggingConfig,
    pub mcp: McpConfig,
    pub lsp: LspConfig,
    pub code_index: CodeIndexConfig,
    pub github: crate::github::GitHubConfig,
}

impl std::fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppConfig")
            .field("api", &self.api)
            .field("deployment_choices", &self.deployment_choices)
            .field("agent", &self.agent)
            .field("ui", &self.ui)
            .field("logging", &self.logging)
            .field("mcp", &self.mcp)
            .field("lsp", &self.lsp)
            .field("code_index", &self.code_index)
            .field("github", &self.github)
            .finish()
    }
}

/// User-selected shape of the Responses endpoint.
///
/// A full URL is sent exactly as configured (apart from the optional
/// `api-version` query parameter). An Azure base URL is safely extended with
/// one `responses` path segment by the URL builder; string concatenation is
/// deliberately avoided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponsesEndpoint {
    FullUrl(String),
    AzureBaseUrl(String),
    OpenAi,
    AwsBedrockRuntime,
}

impl ResponsesEndpoint {
    pub fn resolved_url(&self, allow_insecure_loopback: bool) -> Result<String, ConfigError> {
        match self {
            Self::FullUrl(value) => {
                non_empty_ref("api.responses_url", value)?;
                validate_api_url("api.responses_url", value, allow_insecure_loopback)?;
                Ok(value.clone())
            }
            Self::AzureBaseUrl(value) => responses_url_from_base(value, allow_insecure_loopback),
            Self::OpenAi => Ok("https://api.openai.com/v1/responses".to_owned()),
            Self::AwsBedrockRuntime => Ok("https://bedrock-runtime.amazonaws.com/".to_owned()),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApiProvider {
    #[default]
    Azure,
    OpenAi,
    Google,
    Anthropic,
    /// OpenAI-compatible Bedrock Mantle endpoint authenticated by bearer token.
    AwsBedrock,
    /// Native Bedrock Runtime ConverseStream with the AWS credential chain and SigV4.
    AwsBedrockRuntime,
    OpenRouter,
    XAi,
    Groq,
    Mistral,
    DeepSeek,
    Together,
    Fireworks,
    Cerebras,
    Perplexity,
    Nvidia,
    SambaNova,
    Moonshot,
    Alibaba,
    HuggingFace,
    GitHubModels,
    Ollama,
    Compatible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiWireProtocol {
    Responses,
    ChatCompletions,
    AnthropicMessages,
    GeminiGenerateContent,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ApiTransport {
    #[default]
    Auto,
    Sse,
    WebSocket,
}

impl ApiTransport {
    fn parse(value: Option<&str>) -> Result<Self, ConfigError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("auto") => Ok(Self::Auto),
            Some("sse") | Some("http") => Ok(Self::Sse),
            Some("websocket") | Some("ws") => Ok(Self::WebSocket),
            Some(_) => Err(ConfigError::InvalidValue {
                field: "api.transport",
                message: "must be auto, sse, or websocket".to_owned(),
            }),
        }
    }

    #[must_use]
    pub const fn resolved(self, provider: ApiProvider) -> Self {
        match self {
            Self::Auto if matches!(provider, ApiProvider::OpenAi) => Self::WebSocket,
            Self::Auto => Self::Sse,
            explicit => explicit,
        }
    }
}

impl std::fmt::Display for ApiTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Auto => "auto",
            Self::Sse => "SSE",
            Self::WebSocket => "WebSocket",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApiCapabilities {
    pub images: bool,
    pub files: bool,
    pub audio: bool,
    pub video: bool,
}

impl ApiCapabilities {
    #[must_use]
    pub const fn responses_default() -> Self {
        Self {
            images: true,
            files: true,
            audio: false,
            video: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiAuth {
    ApiKey,
    Bearer,
    AnthropicKey,
    GoogleKey,
    AwsSdk,
}

impl ApiAuth {
    fn parse(value: Option<&str>, provider: ApiProvider) -> Result<Self, ConfigError> {
        let default = match provider {
            ApiProvider::Azure => Self::ApiKey,
            ApiProvider::Anthropic => Self::AnthropicKey,
            ApiProvider::Google => Self::GoogleKey,
            ApiProvider::AwsBedrockRuntime => Self::AwsSdk,
            ApiProvider::OpenAi
            | ApiProvider::AwsBedrock
            | ApiProvider::OpenRouter
            | ApiProvider::XAi
            | ApiProvider::Groq
            | ApiProvider::Mistral
            | ApiProvider::DeepSeek
            | ApiProvider::Together
            | ApiProvider::Fireworks
            | ApiProvider::Cerebras
            | ApiProvider::Perplexity
            | ApiProvider::Nvidia
            | ApiProvider::SambaNova
            | ApiProvider::Moonshot
            | ApiProvider::Alibaba
            | ApiProvider::HuggingFace
            | ApiProvider::GitHubModels
            | ApiProvider::Ollama
            | ApiProvider::Compatible => Self::Bearer,
        };
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None => Ok(default),
            Some("api_key") | Some("api-key") => Ok(Self::ApiKey),
            Some("bearer") => Ok(Self::Bearer),
            Some("anthropic_key") | Some("anthropic-key") => Ok(Self::AnthropicKey),
            Some("google_key") | Some("google-key") => Ok(Self::GoogleKey),
            Some("aws_sdk") | Some("aws-sdk") | Some("sigv4") => Ok(Self::AwsSdk),
            Some(_) => Err(ConfigError::InvalidValue {
                field: "api.provider_auth",
                message: "must be bearer, api_key, anthropic_key, google_key, or aws_sdk"
                    .to_owned(),
            }),
        }
    }
}

impl ApiProvider {
    fn parse(value: Option<&str>) -> Result<Self, ConfigError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("azure") | Some("azure_openai") => Ok(Self::Azure),
            Some("openai") => Ok(Self::OpenAi),
            Some("google") | Some("gemini") => Ok(Self::Google),
            Some("anthropic") | Some("claude") => Ok(Self::Anthropic),
            Some("aws")
            | Some("bedrock")
            | Some("aws_bedrock")
            | Some("bedrock_mantle")
            | Some("aws_bedrock_mantle") => Ok(Self::AwsBedrock),
            Some("bedrock_runtime") | Some("aws_bedrock_runtime") => {
                Ok(Self::AwsBedrockRuntime)
            }
            Some("openrouter") => Ok(Self::OpenRouter),
            Some("xai") | Some("grok") => Ok(Self::XAi),
            Some("groq") => Ok(Self::Groq),
            Some("mistral") => Ok(Self::Mistral),
            Some("deepseek") => Ok(Self::DeepSeek),
            Some("together") => Ok(Self::Together),
            Some("fireworks") => Ok(Self::Fireworks),
            Some("cerebras") => Ok(Self::Cerebras),
            Some("perplexity") => Ok(Self::Perplexity),
            Some("nvidia") | Some("nim") => Ok(Self::Nvidia),
            Some("sambanova") => Ok(Self::SambaNova),
            Some("moonshot") | Some("kimi") => Ok(Self::Moonshot),
            Some("alibaba") | Some("dashscope") | Some("qwen") => Ok(Self::Alibaba),
            Some("huggingface") | Some("hf") => Ok(Self::HuggingFace),
            Some("github") | Some("github_models") => Ok(Self::GitHubModels),
            Some("ollama") | Some("local") => Ok(Self::Ollama),
            Some("compatible") | Some("openai_compatible") => Ok(Self::Compatible),
            Some(_) => Err(ConfigError::InvalidValue {
                field: "api.provider",
                message: "unknown provider; use azure, openai, google, anthropic, bedrock_mantle, bedrock_runtime, openrouter, xai, groq, mistral, deepseek, together, fireworks, cerebras, perplexity, nvidia, sambanova, moonshot, alibaba, huggingface, github_models, ollama, or compatible".to_owned(),
            }),
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Azure => "Azure OpenAI",
            Self::OpenAi => "OpenAI",
            Self::Google => "Google Gemini",
            Self::Anthropic => "Anthropic Claude",
            Self::AwsBedrock => "AWS Bedrock Mantle",
            Self::AwsBedrockRuntime => "AWS Bedrock Runtime",
            Self::OpenRouter => "OpenRouter",
            Self::XAi => "xAI",
            Self::Groq => "Groq",
            Self::Mistral => "Mistral AI",
            Self::DeepSeek => "DeepSeek",
            Self::Together => "Together AI",
            Self::Fireworks => "Fireworks AI",
            Self::Cerebras => "Cerebras",
            Self::Perplexity => "Perplexity",
            Self::Nvidia => "NVIDIA NIM",
            Self::SambaNova => "SambaNova",
            Self::Moonshot => "Moonshot AI",
            Self::Alibaba => "Alibaba DashScope",
            Self::HuggingFace => "Hugging Face",
            Self::GitHubModels => "GitHub Models",
            Self::Ollama => "Ollama",
            Self::Compatible => "OpenAI-compatible",
        }
    }

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Azure => "azure",
            Self::OpenAi => "openai",
            Self::Google => "google",
            Self::Anthropic => "anthropic",
            Self::AwsBedrock => "bedrock-mantle",
            Self::AwsBedrockRuntime => "bedrock-runtime",
            Self::OpenRouter => "openrouter",
            Self::XAi => "xai",
            Self::Groq => "groq",
            Self::Mistral => "mistral",
            Self::DeepSeek => "deepseek",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Cerebras => "cerebras",
            Self::Perplexity => "perplexity",
            Self::Nvidia => "nvidia",
            Self::SambaNova => "sambanova",
            Self::Moonshot => "moonshot",
            Self::Alibaba => "alibaba",
            Self::HuggingFace => "huggingface",
            Self::GitHubModels => "github-models",
            Self::Ollama => "ollama",
            Self::Compatible => "compatible",
        }
    }

    #[must_use]
    pub const fn wire_protocol(self) -> ApiWireProtocol {
        match self {
            Self::Azure | Self::OpenAi | Self::AwsBedrock | Self::Compatible => {
                ApiWireProtocol::Responses
            }
            Self::AwsBedrockRuntime => ApiWireProtocol::Responses,
            Self::Anthropic => ApiWireProtocol::AnthropicMessages,
            Self::Google => ApiWireProtocol::GeminiGenerateContent,
            Self::OpenRouter
            | Self::XAi
            | Self::Groq
            | Self::Mistral
            | Self::DeepSeek
            | Self::Together
            | Self::Fireworks
            | Self::Cerebras
            | Self::Perplexity
            | Self::Nvidia
            | Self::SambaNova
            | Self::Moonshot
            | Self::Alibaba
            | Self::HuggingFace
            | Self::GitHubModels
            | Self::Ollama => ApiWireProtocol::ChatCompletions,
        }
    }

    #[must_use]
    pub const fn capabilities(self) -> ApiCapabilities {
        match self {
            Self::Azure
            | Self::OpenAi
            | Self::AwsBedrock
            | Self::AwsBedrockRuntime
            | Self::Compatible => ApiCapabilities::responses_default(),
            Self::Anthropic => ApiCapabilities {
                images: true,
                files: true,
                audio: false,
                video: false,
            },
            Self::Google => ApiCapabilities {
                images: true,
                files: true,
                audio: true,
                video: true,
            },
            Self::OpenRouter | Self::XAi | Self::Groq => ApiCapabilities {
                images: true,
                files: false,
                audio: false,
                video: false,
            },
            Self::Mistral
            | Self::DeepSeek
            | Self::Together
            | Self::Fireworks
            | Self::Cerebras
            | Self::Perplexity
            | Self::Nvidia
            | Self::SambaNova
            | Self::Moonshot
            | Self::Alibaba
            | Self::HuggingFace
            | Self::GitHubModels
            | Self::Ollama => ApiCapabilities {
                images: false,
                files: false,
                audio: false,
                video: false,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BedrockRuntimeConfig {
    pub region: Option<String>,
    pub profile: Option<String>,
    pub role_arn: Option<String>,
    /// Optional trusted endpoint override for LocalStack/private test endpoints.
    pub endpoint_url: Option<String>,
}

#[derive(Clone)]
pub struct ApiConfig {
    pub provider: ApiProvider,
    pub auth: ApiAuth,
    pub api_key: SecretString,
    pub bedrock_runtime: BedrockRuntimeConfig,
    pub transport: ApiTransport,
    pub endpoint: ResponsesEndpoint,
    /// Permit plaintext HTTP only when the resolved endpoint names a loopback host.
    pub allow_insecure_loopback: bool,
    pub deployment: String,
    pub deployment_choices: Vec<String>,
    pub api_version: Option<String>,
    pub max_output_tokens: u32,
    pub reasoning_effort: ReasoningEffort,
    pub temperature: Option<f32>,
    pub server_compaction_threshold: Option<u64>,
    pub request_timeout: Duration,
    pub stream_idle_timeout: Duration,
    /// Total attempts, including the first request. It is always in `1..=5`.
    pub max_attempts: u32,
    pub retry_min_delay: Duration,
    pub retry_max_delay: Duration,
    /// Numeric Retry-After values are capped to this duration (and never above 120s).
    pub retry_after_cap: Duration,
    pub pricing: PricingCatalog,
    /// Public, credential-free LiteLLM-compatible catalog. `None` disables
    /// network refresh while retaining verified built-ins and manual rates.
    pub pricing_catalog_url: Option<String>,
}

impl std::fmt::Debug for ApiConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiConfig")
            .field("provider", &self.provider)
            .field("auth", &self.auth)
            .field("api_key", &"[REDACTED]")
            .field("bedrock_runtime", &self.bedrock_runtime)
            .field("transport", &self.transport)
            .field("endpoint", &self.endpoint)
            .field("allow_insecure_loopback", &self.allow_insecure_loopback)
            .field("deployment", &self.deployment)
            .field("deployment_choices", &self.deployment_choices)
            .field("api_version", &self.api_version)
            .field("max_output_tokens", &self.max_output_tokens)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("temperature", &self.temperature)
            .field(
                "server_compaction_threshold",
                &self.server_compaction_threshold,
            )
            .field("request_timeout", &self.request_timeout)
            .field("stream_idle_timeout", &self.stream_idle_timeout)
            .field("max_attempts", &self.max_attempts)
            .field("retry_min_delay", &self.retry_min_delay)
            .field("retry_max_delay", &self.retry_max_delay)
            .field("retry_after_cap", &self.retry_after_cap)
            .field("pricing", &self.pricing)
            .field("pricing_catalog_url", &self.pricing_catalog_url)
            .finish()
    }
}

impl ApiConfig {
    /// Validate the public configuration surface. This is deliberately called
    /// both by `AppConfig::load_from` and by the HTTP client because callers
    /// may construct `ApiConfig` directly.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.provider != ApiProvider::AwsBedrockRuntime
            && self.api_key.expose_secret().is_empty()
        {
            return Err(ConfigError::MissingApiKey);
        }
        self.endpoint.resolved_url(self.allow_insecure_loopback)?;
        match (&self.provider, &self.endpoint) {
            (
                ApiProvider::Azure,
                ResponsesEndpoint::FullUrl(_) | ResponsesEndpoint::AzureBaseUrl(_),
            ) => {}
            (ApiProvider::Azure, _) => {
                return Err(ConfigError::InvalidValue {
                    field: "api.endpoint",
                    message: "Azure requires responses_url or azure_base_url".to_owned(),
                });
            }
            (ApiProvider::AwsBedrockRuntime, ResponsesEndpoint::AwsBedrockRuntime) => {}
            (ApiProvider::AwsBedrockRuntime, _) => {
                return Err(ConfigError::InvalidValue {
                    field: "api.endpoint",
                    message: "bedrock_runtime uses the AWS SDK endpoint; use api.bedrock_endpoint_url only for an explicit override".to_owned(),
                });
            }
            (provider, ResponsesEndpoint::AzureBaseUrl(_)) if *provider != ApiProvider::Azure => {
                return Err(ConfigError::InvalidValue {
                    field: "api.endpoint",
                    message: "azure_base_url is valid only for the Azure provider".to_owned(),
                });
            }
            (ApiProvider::OpenAi, ResponsesEndpoint::OpenAi) => {}
            (_, ResponsesEndpoint::OpenAi) => return Err(ConfigError::MissingResponsesUrl),
            (_, ResponsesEndpoint::AwsBedrockRuntime) => {
                return Err(ConfigError::MissingResponsesUrl);
            }
            (_, ResponsesEndpoint::FullUrl(_)) => {}
            (_, ResponsesEndpoint::AzureBaseUrl(_)) => {
                return Err(ConfigError::InvalidValue {
                    field: "api.endpoint",
                    message: "azure_base_url is valid only for the Azure provider".to_owned(),
                });
            }
        }
        let auth_valid = match self.provider {
            ApiProvider::Azure => self.auth == ApiAuth::ApiKey,
            ApiProvider::Anthropic => self.auth == ApiAuth::AnthropicKey,
            ApiProvider::Google => self.auth == ApiAuth::GoogleKey,
            ApiProvider::AwsBedrockRuntime => self.auth == ApiAuth::AwsSdk,
            ApiProvider::Compatible => {
                matches!(self.auth, ApiAuth::ApiKey | ApiAuth::Bearer)
            }
            ApiProvider::OpenAi
            | ApiProvider::AwsBedrock
            | ApiProvider::OpenRouter
            | ApiProvider::XAi
            | ApiProvider::Groq
            | ApiProvider::Mistral
            | ApiProvider::DeepSeek
            | ApiProvider::Together
            | ApiProvider::Fireworks
            | ApiProvider::Cerebras
            | ApiProvider::Perplexity
            | ApiProvider::Nvidia
            | ApiProvider::SambaNova
            | ApiProvider::Moonshot
            | ApiProvider::Alibaba
            | ApiProvider::HuggingFace
            | ApiProvider::GitHubModels
            | ApiProvider::Ollama => self.auth == ApiAuth::Bearer,
        };
        if !auth_valid {
            return Err(ConfigError::InvalidValue {
                field: "api.provider_auth",
                message: format!(
                    "authentication mode is not valid for {}",
                    self.provider.label()
                ),
            });
        }
        if self.transport.resolved(self.provider) == ApiTransport::WebSocket
            && self.provider != ApiProvider::OpenAi
        {
            return Err(ConfigError::InvalidValue {
                field: "api.transport",
                message: "websocket is currently supported only by the official OpenAI Responses provider; Azure, Bedrock and compatible providers remain on SSE"
                    .to_owned(),
            });
        }
        if self.provider != ApiProvider::Azure && self.api_version.is_some() {
            return Err(ConfigError::InvalidValue {
                field: "api.api_version",
                message: "api_version is Azure-only".to_owned(),
            });
        }
        for (field, value, limit) in [
            ("api.aws_region", self.bedrock_runtime.region.as_deref(), 64),
            (
                "api.aws_profile",
                self.bedrock_runtime.profile.as_deref(),
                128,
            ),
            (
                "api.aws_role_arn",
                self.bedrock_runtime.role_arn.as_deref(),
                2_048,
            ),
        ] {
            if let Some(value) = value
                && (value.trim().is_empty()
                    || value.len() > limit
                    || value.chars().any(char::is_control))
            {
                return Err(ConfigError::InvalidValue {
                    field,
                    message: "must be a non-empty bounded visible value".to_owned(),
                });
            }
        }
        if let Some(endpoint) = &self.bedrock_runtime.endpoint_url {
            validate_api_url(
                "api.bedrock_endpoint_url",
                endpoint,
                self.allow_insecure_loopback,
            )?;
        }
        if let Some(url) = &self.pricing_catalog_url {
            validate_api_url("api.pricing_catalog_url", url, false)?;
        }
        non_empty_ref("api.deployment", &self.deployment)?;
        if self.deployment_choices.is_empty()
            || self.deployment_choices.len() > 32
            || !self
                .deployment_choices
                .iter()
                .any(|choice| choice == &self.deployment)
            || self.deployment_choices.iter().any(|choice| {
                choice.trim().is_empty()
                    || choice.len() > 256
                    || choice.chars().any(char::is_control)
            })
        {
            return Err(ConfigError::InvalidValue {
                field: "api.deployment_choices",
                message: "must contain 1..=32 visible deployment names including api.deployment"
                    .to_owned(),
            });
        }
        require_positive("api.max_output_tokens", self.max_output_tokens)?;
        if let Some(temperature) = self.temperature
            && (!temperature.is_finite() || !(0.0..=2.0).contains(&temperature))
        {
            return Err(ConfigError::InvalidValue {
                field: "api.temperature",
                message: "must be finite and between 0 and 2 inclusive".to_owned(),
            });
        }
        if self.server_compaction_threshold == Some(0) {
            return Err(ConfigError::InvalidValue {
                field: "api.server_compaction_threshold",
                message: "must be greater than zero when set".to_owned(),
            });
        }
        if self.server_compaction_threshold.is_some()
            && self.provider.wire_protocol() != ApiWireProtocol::Responses
        {
            return Err(ConfigError::InvalidValue {
                field: "api.server_compaction_threshold",
                message: "server compaction is available only with the Responses protocol"
                    .to_owned(),
            });
        }
        validate_timeout("api.request_timeout", self.request_timeout)?;
        validate_timeout("api.stream_idle_timeout", self.stream_idle_timeout)?;
        validate_timeout("api.retry_min_delay", self.retry_min_delay)?;
        validate_timeout("api.retry_max_delay", self.retry_max_delay)?;
        validate_timeout("api.retry_after_cap", self.retry_after_cap)?;
        if !(1..=5).contains(&self.max_attempts) {
            return Err(ConfigError::InvalidValue {
                field: "api.max_attempts",
                message: "must be between 1 and 5 inclusive".to_owned(),
            });
        }
        if self.retry_min_delay > self.retry_max_delay {
            return Err(ConfigError::InvalidValue {
                field: "api.retry_min_delay",
                message: "must not exceed api.retry_max_delay".to_owned(),
            });
        }
        if self.retry_max_delay > Duration::from_secs(DEFAULT_RETRY_MAX_DELAY_SECS) {
            return Err(ConfigError::InvalidValue {
                field: "api.retry_max_delay",
                message: format!("must not exceed {DEFAULT_RETRY_MAX_DELAY_SECS}s"),
            });
        }
        if self.retry_after_cap > Duration::from_secs(MAX_RETRY_AFTER_SECS) {
            return Err(ConfigError::InvalidValue {
                field: "api.retry_after_cap",
                message: format!("must not exceed {MAX_RETRY_AFTER_SECS}s"),
            });
        }
        Ok(())
    }

    /// Exact Responses API `context_management` value for optional server
    /// compaction. Keeping this builder next to validation prevents drift
    /// between TOML/env parsing and the wire shape.
    #[must_use]
    pub fn context_management(&self) -> Option<Vec<ContextManagement>> {
        self.server_compaction_threshold.map(|compact_threshold| {
            vec![serde_json::json!({
                "type": "compaction",
                "compact_threshold": compact_threshold,
            })]
        })
    }
}

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub context_mode: ContextMode,
    pub context_budget: u32,
    /// User-declared context-window capability for the selected deployment.
    /// Runtime controls can lower the budget but never exceed this ceiling.
    pub max_context_budget: u32,
    pub max_tool_iterations: u32,
    pub workspace_root: PathBuf,
    pub session_dir: PathBuf,
    /// Trusted per-user additive deny rules. The project contributes a second
    /// additive source through `.decodeignore` inside the workspace.
    pub privacy_user_rules_file: PathBuf,
    pub instructions_file: PathBuf,
    /// Exact UTF-8 contents of `instructions_file`; no trimming or newline rewriting.
    pub instructions: String,
    pub project_instructions: ProjectInstructionsConfig,
    pub skills: SkillsConfig,
    pub exec_timeout: Duration,
    pub subagents: SubagentConfig,
    pub shell: ShellConfig,
    pub whip: WhipConfig,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillsConfig {
    pub enabled: bool,
    pub project_enabled: bool,
    /// Trusted per-user skill root. Repository configuration is deliberately
    /// unable to change this path.
    pub user_dir: PathBuf,
    pub metadata_budget_bytes: usize,
    pub max_skills: usize,
    pub max_skill_bytes: usize,
    pub max_resource_bytes: usize,
    pub max_resources: usize,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            project_enabled: true,
            user_dir: directories::ProjectDirs::from("dev", "denysoid", "decode")
                .map(|dirs| dirs.config_dir().join("skills"))
                .unwrap_or_else(|| std::env::temp_dir().join("decode-skills")),
            metadata_budget_bytes: DEFAULT_SKILLS_METADATA_BUDGET_BYTES,
            max_skills: DEFAULT_SKILLS_MAX_SKILLS,
            max_skill_bytes: DEFAULT_SKILLS_MAX_SKILL_BYTES,
            max_resource_bytes: DEFAULT_SKILLS_MAX_RESOURCE_BYTES,
            max_resources: DEFAULT_SKILLS_MAX_RESOURCES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectInstructionsConfig {
    pub enabled: bool,
    pub max_source_bytes: usize,
    pub max_total_bytes: usize,
    pub max_sources: usize,
    pub max_include_depth: usize,
}

impl Default for ProjectInstructionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_source_bytes: DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCE_BYTES,
            max_total_bytes: DEFAULT_PROJECT_INSTRUCTION_MAX_TOTAL_BYTES,
            max_sources: DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCES,
            max_include_depth: DEFAULT_PROJECT_INSTRUCTION_MAX_INCLUDE_DEPTH,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubagentConfig {
    pub enabled: bool,
    pub allow_mcp: bool,
    pub worktree_dir: PathBuf,
    pub max_parallel: u16,
    pub max_per_session: u16,
    pub max_tool_iterations: u32,
    /// Hard billed-token ceiling for one delegated worker, including every
    /// request it makes while recursively coordinating descendants.
    pub max_tokens_per_agent: u64,
    /// Hard billed-token ceiling shared by the complete delegated tree in one
    /// main-session. Concurrent requests reserve capacity before dispatch.
    pub max_total_tokens_per_session: u64,
    /// Root agent is depth 0; directly delegated workers are depth 1.
    pub max_depth: u8,
    pub max_children_per_agent: u16,
    pub task_timeout: Duration,
    pub git_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct ShellConfig {
    pub confirmation_mode: ShellConfirmationMode,
    pub timeout_rules: Vec<ShellTimeoutRule>,
    pub direct_exec_allowlist: Vec<StrictAllowlistEntry>,
    pub terminal: InteractiveTerminalConfig,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            confirmation_mode: ShellConfirmationMode::Always,
            timeout_rules: Vec::new(),
            direct_exec_allowlist: Vec::new(),
            terminal: InteractiveTerminalConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveTerminalConfig {
    pub enabled: bool,
    /// Explicit trusted shell executable. `None` selects the platform default.
    pub program: Option<String>,
    pub args: Vec<String>,
    pub max_sessions: usize,
    pub scrollback_lines: usize,
}

impl Default for InteractiveTerminalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            program: None,
            args: Vec::new(),
            max_sessions: DEFAULT_TERMINAL_MAX_SESSIONS,
            scrollback_lines: DEFAULT_TERMINAL_SCROLLBACK_LINES,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ShellTimeoutRule {
    pub prefix: String,
    pub timeout: Duration,
}

impl ShellConfig {
    #[must_use]
    pub fn timeout_for(&self, command: &str, fallback: Duration) -> Duration {
        self.timeout_rules
            .iter()
            .find(|rule| command.trim_start().starts_with(&rule.prefix))
            .map_or(fallback, |rule| rule.timeout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextMode {
    Stateless,
    Stateful,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected 'stateless' or 'stateful', got {value:?}")]
pub struct ParseContextModeError {
    value: String,
}

impl FromStr for ContextMode {
    type Err = ParseContextModeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "stateless" => Ok(Self::Stateless),
            "stateful" => Ok(Self::Stateful),
            _ => Err(ParseContextModeError {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Clone, Debug)]
pub struct WhipConfig {
    pub enabled: bool,
    pub hotkey: String,
    pub double_hit_window: Duration,
    pub penalty_completed_responses: u32,
    pub max_output_percent: u8,
    pub minimum_output_tokens: u32,
}

impl Default for WhipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hotkey: "w".to_owned(),
            double_hit_window: Duration::from_millis(2_000),
            penalty_completed_responses: 3,
            max_output_percent: 60,
            minimum_output_tokens: 256,
        }
    }
}

#[derive(Clone, Debug)]
pub struct UiConfig {
    pub confirm_destructive: bool,
    pub mouse_enabled: bool,
    pub language: UiLanguage,
    pub onboarding_completed: bool,
    pub mascot_enabled: bool,
    pub show_thinking: bool,
    pub show_tool_activity: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UiLanguage {
    #[default]
    English,
    Russian,
    Ukrainian,
    Spanish,
    German,
    French,
    Polish,
    Portuguese,
    Chinese,
    Japanese,
    Korean,
    Turkish,
}

impl UiLanguage {
    pub(crate) fn parse(value: Option<&str>) -> Result<Self, ConfigError> {
        match value.map(str::trim).filter(|value| !value.is_empty()) {
            None | Some("en") | Some("english") => Ok(Self::English),
            Some("ru") | Some("russian") | Some("русский") => Ok(Self::Russian),
            Some("uk") | Some("ua") | Some("ukrainian") | Some("українська") => {
                Ok(Self::Ukrainian)
            }
            Some("es") | Some("spanish") => Ok(Self::Spanish),
            Some("de") | Some("german") => Ok(Self::German),
            Some("fr") | Some("french") => Ok(Self::French),
            Some("pl") | Some("polish") => Ok(Self::Polish),
            Some("pt") | Some("portuguese") => Ok(Self::Portuguese),
            Some("zh") | Some("chinese") => Ok(Self::Chinese),
            Some("ja") | Some("japanese") => Ok(Self::Japanese),
            Some("ko") | Some("korean") => Ok(Self::Korean),
            Some("tr") | Some("turkish") => Ok(Self::Turkish),
            Some(_) => Err(ConfigError::InvalidValue {
                field: "ui.language",
                message: "use en, ru, uk, es, de, fr, pl, pt, zh, ja, ko, or tr".to_owned(),
            }),
        }
    }

    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Russian => "ru",
            Self::Ukrainian => "uk",
            Self::Spanish => "es",
            Self::German => "de",
            Self::French => "fr",
            Self::Polish => "pl",
            Self::Portuguese => "pt",
            Self::Chinese => "zh",
            Self::Japanese => "ja",
            Self::Korean => "ko",
            Self::Turkish => "tr",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::Russian => "Русский",
            Self::Ukrainian => "Українська",
            Self::Spanish => "Español",
            Self::German => "Deutsch",
            Self::French => "Français",
            Self::Polish => "Polski",
            Self::Portuguese => "Português",
            Self::Chinese => "中文",
            Self::Japanese => "日本語",
            Self::Korean => "한국어",
            Self::Turkish => "Türkçe",
        }
    }

    pub const ALL: [Self; 12] = [
        Self::English,
        Self::Russian,
        Self::Ukrainian,
        Self::Spanish,
        Self::German,
        Self::French,
        Self::Polish,
        Self::Portuguese,
        Self::Chinese,
        Self::Japanese,
        Self::Korean,
        Self::Turkish,
    ];
}

#[derive(Clone, Debug)]
pub struct LoggingConfig {
    pub level: String,
    pub dir: PathBuf,
}

impl AppConfig {
    pub fn load() -> Result<Self, ConfigError> {
        // Environment variables must come from the invoking process. Loading a
        // workspace-controlled .env file here could silently redirect requests
        // carrying the user's credential.
        let matches = CliArgs::command().get_matches();
        let mut args =
            CliArgs::from_arg_matches(&matches).map_err(|error| ConfigError::InvalidValue {
                field: "cli",
                message: error.to_string(),
            })?;

        // Clap correctly resolves CLI > env for one argument, but the two URL
        // forms are mutually exclusive aliases at the application layer. Keep
        // the command-line form and discard the lower-precedence env form;
        // only two values originating from the same layer are a conflict.
        match (
            matches.value_source("responses_url"),
            matches.value_source("azure_base_url"),
        ) {
            (Some(ValueSource::CommandLine), Some(ValueSource::EnvVariable)) => {
                args.azure_base_url = None;
            }
            (Some(ValueSource::EnvVariable), Some(ValueSource::CommandLine)) => {
                args.responses_url = None;
            }
            _ => {}
        }
        match (
            matches.value_source("model"),
            matches.value_source("deployment"),
        ) {
            (Some(ValueSource::CommandLine), Some(ValueSource::EnvVariable)) => {
                args.deployment = None;
            }
            (Some(ValueSource::EnvVariable), Some(ValueSource::CommandLine)) => {
                args.model = None;
            }
            _ => {}
        }
        Self::load_from(args)
    }

    pub fn load_from(args: CliArgs) -> Result<Self, ConfigError> {
        let file_cfg =
            Self::load_file_config(args.config_file.as_deref(), args.workspace.as_deref())?;
        let file_api = file_cfg.api.unwrap_or_default();
        let file_agent = file_cfg.agent.unwrap_or_default();
        let file_ui = file_cfg.ui.unwrap_or_default();
        let file_log = file_cfg.logging.unwrap_or_default();
        let mut file_mcp = file_cfg.mcp.unwrap_or_default();
        let mut file_lsp = file_cfg.lsp.unwrap_or_default();
        let file_code_index = file_cfg.code_index.unwrap_or_default();
        let file_github = file_cfg.github.unwrap_or_default();
        let provider =
            ApiProvider::parse(args.provider.as_deref().or(file_api.provider.as_deref()))?;
        let auth = ApiAuth::parse(
            args.provider_auth
                .as_deref()
                .or(file_api.provider_auth.as_deref()),
            provider,
        )?;
        let api_transport = ApiTransport::parse(
            args.api_transport
                .as_deref()
                .or(file_api.transport.as_deref()),
        )?;

        if args.responses_url.is_some() && args.azure_base_url.is_some() {
            return Err(ConfigError::InvalidValue {
                field: "api.endpoint",
                message: "responses_url and azure_base_url cannot both be set by CLI/environment"
                    .to_owned(),
            });
        }
        if file_api.responses_url.is_some() && file_api.azure_base_url.is_some() {
            return Err(ConfigError::InvalidValue {
                field: "api.endpoint",
                message: "responses_url and azure_base_url cannot both be set in one config file"
                    .to_owned(),
            });
        }
        let pricing = build_pricing_catalog(provider, &file_api.pricing)?;
        let pricing_catalog_url = file_api.auto_pricing.unwrap_or(true).then(|| {
            file_api.pricing_catalog_url.unwrap_or_else(|| {
                "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
                    .to_owned()
            })
        });

        let explicit_workspace = args.workspace.is_some();
        let workspace_candidate = args
            .workspace
            .clone()
            .or_else(|| file_agent.workspace_root.clone())
            .unwrap_or(std::env::current_dir().map_err(ConfigError::CurrentDirectory)?);
        let workspace_root = canonical_directory("agent.workspace_root", &workspace_candidate)?;
        let workspace_root = if explicit_workspace {
            workspace_root
        } else {
            build_project_root(&workspace_root).unwrap_or(workspace_root)
        };
        let session_dir = args
            .session_dir
            .or(file_agent.session_dir)
            .unwrap_or_else(|| {
                directories::ProjectDirs::from("dev", "denysoid", "decode")
                    .map(|dirs| dirs.data_local_dir().join("sessions"))
                    .unwrap_or_else(|| workspace_root.join(".git").join("decode").join("sessions"))
            });
        let privacy_user_rules_file = directories::ProjectDirs::from("dev", "denysoid", "decode")
            .map(|dirs| dirs.config_dir().join(crate::privacy::USER_RULES_FILE))
            .unwrap_or_else(|| {
                session_dir
                    .parent()
                    .unwrap_or(&session_dir)
                    .join(crate::privacy::USER_RULES_FILE)
            });

        let env_file_key = if provider == ApiProvider::AwsBedrockRuntime {
            None
        } else {
            args.env_file
                .as_deref()
                .map(|path| read_explicit_env_key(path, &workspace_root, provider))
                .transpose()?
        };
        // Credentials are accepted only from the invoking process or an
        // explicitly selected, outside-workspace credential file. Process
        // environment wins without allowing the file to set any other key.
        let keyring_key = if provider == ApiProvider::AwsBedrockRuntime {
            None
        } else {
            file_api
                .keyring_account
                .as_deref()
                .map(read_provider_keyring)
                .transpose()?
                .flatten()
        };
        let api_key_raw = if provider == ApiProvider::AwsBedrockRuntime {
            String::new()
        } else {
            provider_api_key_envs(provider)
                .iter()
                .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
                .or(env_file_key)
                .or(keyring_key)
                .or_else(|| (provider == ApiProvider::Ollama).then(|| "ollama-local".to_owned()))
                .ok_or(ConfigError::MissingApiKey)?
        };
        let api_key = SecretString::new(api_key_raw.into());

        let allow_insecure_loopback = args
            .allow_insecure_loopback
            .or(file_api.allow_insecure_loopback)
            .unwrap_or(provider == ApiProvider::Ollama);
        // Preserve source precedence across the two equivalent URL forms: an
        // environment/CLI base URL must beat a full URL from TOML, while a full
        // URL wins over a base URL from the same source.
        let endpoint = if let Some(url) = args.responses_url {
            ResponsesEndpoint::FullUrl(non_empty("api.responses_url", url)?)
        } else if let Some(base_url) = args.azure_base_url {
            ResponsesEndpoint::AzureBaseUrl(non_empty("api.azure_base_url", base_url)?)
        } else if let Some(url) = file_api.responses_url {
            endpoint_from_stored_responses_url(provider, non_empty("api.responses_url", url)?)
        } else if let Some(base_url) = file_api.azure_base_url {
            ResponsesEndpoint::AzureBaseUrl(non_empty("api.azure_base_url", base_url)?)
        } else if provider == ApiProvider::OpenAi {
            ResponsesEndpoint::OpenAi
        } else if provider == ApiProvider::AwsBedrockRuntime {
            ResponsesEndpoint::AwsBedrockRuntime
        } else if let Some(url) = default_provider_endpoint(provider) {
            ResponsesEndpoint::FullUrl(url.to_owned())
        } else {
            return Err(ConfigError::MissingResponsesUrl);
        };
        endpoint.resolved_url(allow_insecure_loopback)?;
        let deployment = non_empty(
            "api.deployment",
            args.model
                .or(args.deployment)
                .or(file_api.model)
                .or(file_api.deployment)
                .ok_or(ConfigError::MissingDeployment)?,
        )?;
        let mut deployment_choices = args
            .deployment_choices
            .or(file_api.deployment_choices)
            .unwrap_or_default()
            .into_iter()
            .map(|choice| choice.trim().to_owned())
            .filter(|choice| !choice.is_empty())
            .collect::<Vec<_>>();
        let mut seen_deployments = HashSet::new();
        deployment_choices.retain(|choice| seen_deployments.insert(choice.clone()));
        if !deployment_choices
            .iter()
            .any(|choice| choice == &deployment)
        {
            deployment_choices.insert(0, deployment.clone());
        }
        if deployment_choices.len() > 32 {
            return Err(ConfigError::InvalidValue {
                field: "api.deployment_choices",
                message: "must contain at most 32 deployment names".to_owned(),
            });
        }
        let api_version = args
            .api_version
            .or(file_api.api_version)
            .and_then(|value| (!value.trim().is_empty()).then_some(value));
        let max_output_tokens = args
            .max_output_tokens
            .or(file_api.max_output_tokens)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        require_positive("api.max_output_tokens", max_output_tokens)?;
        let reasoning_effort = args
            .reasoning_effort
            .or(file_api.reasoning_effort)
            .unwrap_or_else(|| "medium".to_owned())
            .parse::<ReasoningEffort>()
            .map_err(|error| ConfigError::InvalidValue {
                field: "api.reasoning_effort",
                message: error.to_string(),
            })?;
        let temperature = args.temperature.or(file_api.temperature);
        if let Some(temperature) = temperature
            && (!temperature.is_finite() || !(0.0..=2.0).contains(&temperature))
        {
            return Err(ConfigError::InvalidValue {
                field: "api.temperature",
                message: "must be finite and between 0 and 2 inclusive".to_owned(),
            });
        }
        let server_compaction_threshold = args
            .server_compaction_threshold
            .or(file_api.server_compaction_threshold);
        if server_compaction_threshold == Some(0) {
            return Err(ConfigError::InvalidValue {
                field: "api.server_compaction_threshold",
                message: "must be greater than zero when set".to_owned(),
            });
        }
        let request_timeout_secs = args
            .request_timeout_secs
            .or(file_api.request_timeout_secs)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
        require_positive("api.request_timeout_secs", request_timeout_secs)?;
        require_at_most(
            "api.request_timeout_secs",
            request_timeout_secs,
            MAX_TIMEOUT_SECS,
        )?;
        let stream_idle_timeout_secs = args
            .stream_idle_timeout_secs
            .or(file_api.stream_idle_timeout_secs)
            .unwrap_or(DEFAULT_STREAM_IDLE_TIMEOUT_SECS);
        require_positive("api.stream_idle_timeout_secs", stream_idle_timeout_secs)?;
        require_at_most(
            "api.stream_idle_timeout_secs",
            stream_idle_timeout_secs,
            MAX_TIMEOUT_SECS,
        )?;
        let max_attempts = args
            .max_attempts
            .or(file_api.max_attempts)
            .unwrap_or(DEFAULT_MAX_ATTEMPTS);
        if !(1..=5).contains(&max_attempts) {
            return Err(ConfigError::InvalidValue {
                field: "api.max_attempts",
                message: "must be between 1 and 5 inclusive".to_owned(),
            });
        }
        let retry_min_delay_ms = args
            .retry_min_delay_ms
            .or(file_api.retry_min_delay_ms)
            .unwrap_or(DEFAULT_RETRY_MIN_DELAY_MS);
        require_positive("api.retry_min_delay_ms", retry_min_delay_ms)?;
        let retry_max_delay_secs = args
            .retry_max_delay_secs
            .or(file_api.retry_max_delay_secs)
            .unwrap_or(DEFAULT_RETRY_MAX_DELAY_SECS);
        require_positive("api.retry_max_delay_secs", retry_max_delay_secs)?;
        require_at_most(
            "api.retry_max_delay_secs",
            retry_max_delay_secs,
            DEFAULT_RETRY_MAX_DELAY_SECS,
        )?;
        let retry_min_delay = Duration::from_millis(retry_min_delay_ms);
        let retry_max_delay = Duration::from_secs(retry_max_delay_secs);
        if retry_min_delay > retry_max_delay {
            return Err(ConfigError::InvalidValue {
                field: "api.retry_min_delay_ms",
                message: "must not exceed api.retry_max_delay_secs".to_owned(),
            });
        }
        let retry_after_cap_secs = args
            .retry_after_cap_secs
            .or(file_api.retry_after_cap_secs)
            .unwrap_or(MAX_RETRY_AFTER_SECS);
        require_positive("api.retry_after_cap_secs", retry_after_cap_secs)?;
        require_at_most(
            "api.retry_after_cap_secs",
            retry_after_cap_secs,
            MAX_RETRY_AFTER_SECS,
        )?;
        let retry_after_cap = Duration::from_secs(retry_after_cap_secs);

        let context_mode = args
            .context_mode
            .or(file_agent.context_mode)
            .unwrap_or_else(|| "stateless".to_owned())
            .parse::<ContextMode>()
            .map_err(|error| ConfigError::InvalidValue {
                field: "agent.context_mode",
                message: error.to_string(),
            })?;
        let context_budget = args
            .context_budget
            .or(file_agent.context_budget)
            .unwrap_or(DEFAULT_CONTEXT_BUDGET);
        require_positive("agent.context_budget", context_budget)?;
        let max_context_budget = args
            .max_context_budget
            .or(file_agent.max_context_budget)
            .unwrap_or(context_budget);
        require_positive("agent.max_context_budget", max_context_budget)?;
        require_at_most(
            "agent.max_context_budget",
            u64::from(max_context_budget),
            u64::from(MAX_CONTEXT_BUDGET),
        )?;
        if context_budget > max_context_budget {
            return Err(ConfigError::InvalidValue {
                field: "agent.context_budget",
                message: "must not exceed agent.max_context_budget".to_owned(),
            });
        }
        let max_tool_iterations = args
            .max_tool_iterations
            .or(file_agent.max_tool_iterations)
            .unwrap_or(DEFAULT_MAX_TOOL_ITERATIONS);
        require_positive("agent.max_tool_iterations", max_tool_iterations)?;
        require_at_most(
            "agent.max_tool_iterations",
            u64::from(max_tool_iterations),
            u64::from(MAX_TOOL_ITERATIONS),
        )?;

        let file_project_instructions = file_agent.project_instructions.unwrap_or_default();
        let project_instructions = ProjectInstructionsConfig {
            enabled: file_project_instructions.enabled.unwrap_or(true),
            max_source_bytes: file_project_instructions
                .max_source_bytes
                .unwrap_or(DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCE_BYTES),
            max_total_bytes: file_project_instructions
                .max_total_bytes
                .unwrap_or(DEFAULT_PROJECT_INSTRUCTION_MAX_TOTAL_BYTES),
            max_sources: file_project_instructions
                .max_sources
                .unwrap_or(DEFAULT_PROJECT_INSTRUCTION_MAX_SOURCES),
            max_include_depth: file_project_instructions
                .max_include_depth
                .unwrap_or(DEFAULT_PROJECT_INSTRUCTION_MAX_INCLUDE_DEPTH),
        };
        validate_project_instructions(&project_instructions)?;

        let file_skills = file_agent.skills.unwrap_or_default();
        let default_skills_dir = directories::ProjectDirs::from("dev", "denysoid", "decode")
            .map(|dirs| dirs.config_dir().join("skills"))
            .unwrap_or_else(|| std::env::temp_dir().join("decode-skills"));
        let skills_user_dir = file_skills.user_dir.unwrap_or(default_skills_dir);
        if !skills_user_dir.is_absolute() {
            return Err(ConfigError::InvalidValue {
                field: "agent.skills.user_dir",
                message: "must be an absolute path from trusted configuration".to_owned(),
            });
        }
        if skills_user_dir.starts_with(&workspace_root) {
            return Err(ConfigError::InvalidValue {
                field: "agent.skills.user_dir",
                message: "must be outside the workspace; project skills belong in .decode/skills"
                    .to_owned(),
            });
        }
        let skills = SkillsConfig {
            enabled: file_skills.enabled.unwrap_or(true),
            project_enabled: file_skills.project_enabled.unwrap_or(true),
            user_dir: skills_user_dir,
            metadata_budget_bytes: file_skills
                .metadata_budget_bytes
                .unwrap_or(DEFAULT_SKILLS_METADATA_BUDGET_BYTES),
            max_skills: file_skills.max_skills.unwrap_or(DEFAULT_SKILLS_MAX_SKILLS),
            max_skill_bytes: file_skills
                .max_skill_bytes
                .unwrap_or(DEFAULT_SKILLS_MAX_SKILL_BYTES),
            max_resource_bytes: file_skills
                .max_resource_bytes
                .unwrap_or(DEFAULT_SKILLS_MAX_RESOURCE_BYTES),
            max_resources: file_skills
                .max_resources
                .unwrap_or(DEFAULT_SKILLS_MAX_RESOURCES),
        };
        validate_skills(&skills)?;

        let instructions_candidate = args
            .instructions_file
            .or(file_agent.instructions_file)
            .ok_or(ConfigError::MissingInstructionsFile)?;
        if !instructions_candidate.is_absolute() {
            return Err(ConfigError::InvalidValue {
                field: "agent.instructions_file",
                message: "must be an absolute path from a trusted source".to_owned(),
            });
        }
        let link_metadata =
            std::fs::symlink_metadata(&instructions_candidate).map_err(|source| {
                ConfigError::InstructionsIo {
                    path: instructions_candidate.clone(),
                    source,
                }
            })?;
        if link_metadata.file_type().is_symlink() {
            return Err(ConfigError::InstructionsSymlink(instructions_candidate));
        }
        let instructions_file =
            std::fs::canonicalize(&instructions_candidate).map_err(|source| {
                ConfigError::InstructionsIo {
                    path: instructions_candidate.clone(),
                    source,
                }
            })?;
        if !instructions_file.is_file() {
            return Err(ConfigError::InstructionsNotFile(instructions_file));
        }
        let metadata = std::fs::metadata(&instructions_file).map_err(|source| {
            ConfigError::InstructionsIo {
                path: instructions_file.clone(),
                source,
            }
        })?;
        if metadata.len() > MAX_INSTRUCTIONS_BYTES {
            return Err(instructions_too_large(metadata.len()));
        }
        let instructions = read_instructions_bounded(&instructions_file)?;
        let exec_timeout_secs = args
            .exec_timeout_secs
            .or(file_agent.exec_timeout_secs)
            .unwrap_or(DEFAULT_EXEC_TIMEOUT_SECS);
        require_positive("agent.exec_timeout_secs", exec_timeout_secs)?;
        require_at_most(
            "agent.exec_timeout_secs",
            exec_timeout_secs,
            MAX_TIMEOUT_SECS,
        )?;

        let file_subagents = file_agent.subagents.unwrap_or_default();
        let subagents_enabled = args
            .subagents_enabled
            .or(file_subagents.enabled)
            .unwrap_or(true);
        let default_worktree_dir = session_dir
            .parent()
            .map(|parent| parent.join("worktrees"))
            .filter(|path| !path.starts_with(&workspace_root))
            .unwrap_or_else(|| std::env::temp_dir().join("decode-worktrees"));
        let subagent_worktree_dir = args
            .subagent_worktree_dir
            .or(file_subagents.worktree_dir)
            .unwrap_or(default_worktree_dir);
        if !subagent_worktree_dir.is_absolute() {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.worktree_dir",
                message: "must be an absolute path from trusted configuration".to_owned(),
            });
        }
        if subagent_worktree_dir.starts_with(&workspace_root) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.worktree_dir",
                message: "must be outside the workspace to avoid recursive tool traversal"
                    .to_owned(),
            });
        }
        let subagent_max_parallel = args
            .subagent_max_parallel
            .or(file_subagents.max_parallel)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_PARALLEL);
        if !(1..=16).contains(&subagent_max_parallel) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_parallel",
                message: "must be between 1 and 16 inclusive".to_owned(),
            });
        }
        let subagent_max_per_session = args
            .subagent_max_per_session
            .or(file_subagents.max_per_session)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_PER_SESSION);
        if !(1..=64).contains(&subagent_max_per_session)
            || subagent_max_per_session < subagent_max_parallel
        {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_per_session",
                message: "must be between max_parallel and 64 inclusive".to_owned(),
            });
        }
        let subagent_max_tool_iterations = args
            .subagent_max_tool_iterations
            .or(file_subagents.max_tool_iterations)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TOOL_ITERATIONS);
        if !(1..=100).contains(&subagent_max_tool_iterations) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_tool_iterations",
                message: "must be between 1 and 100 inclusive".to_owned(),
            });
        }
        let subagent_max_tokens_per_agent = args
            .subagent_max_tokens_per_agent
            .or(file_subagents.max_tokens_per_agent)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TOKENS_PER_AGENT);
        if !(1_024..=MAX_SUBAGENT_TOKEN_BUDGET).contains(&subagent_max_tokens_per_agent) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_tokens_per_agent",
                message: format!("must be between 1024 and {MAX_SUBAGENT_TOKEN_BUDGET} inclusive"),
            });
        }
        let subagent_max_total_tokens_per_session = args
            .subagent_max_total_tokens_per_session
            .or(file_subagents.max_total_tokens_per_session)
            .unwrap_or(DEFAULT_SUBAGENT_MAX_TOTAL_TOKENS_PER_SESSION);
        if subagent_max_total_tokens_per_session < subagent_max_tokens_per_agent
            || subagent_max_total_tokens_per_session > MAX_SUBAGENT_TOKEN_BUDGET
        {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_total_tokens_per_session",
                message: format!(
                    "must be between max_tokens_per_agent and {MAX_SUBAGENT_TOKEN_BUDGET} inclusive"
                ),
            });
        }
        let subagent_max_depth = file_subagents
            .max_depth
            .unwrap_or(DEFAULT_SUBAGENT_MAX_DEPTH);
        if !(1..=4).contains(&subagent_max_depth) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_depth",
                message: "must be between 1 and 4 inclusive".to_owned(),
            });
        }
        let subagent_max_children = file_subagents
            .max_children_per_agent
            .unwrap_or(DEFAULT_SUBAGENT_MAX_CHILDREN);
        if !(1..=16).contains(&subagent_max_children) {
            return Err(ConfigError::InvalidValue {
                field: "agent.subagents.max_children_per_agent",
                message: "must be between 1 and 16 inclusive".to_owned(),
            });
        }
        let subagent_task_timeout_secs = args
            .subagent_task_timeout_secs
            .or(file_subagents.task_timeout_secs)
            .unwrap_or(DEFAULT_SUBAGENT_TASK_TIMEOUT_SECS);
        require_positive(
            "agent.subagents.task_timeout_secs",
            subagent_task_timeout_secs,
        )?;
        require_at_most(
            "agent.subagents.task_timeout_secs",
            subagent_task_timeout_secs,
            MAX_TIMEOUT_SECS,
        )?;
        let subagent_git_timeout_secs = args
            .subagent_git_timeout_secs
            .or(file_subagents.git_timeout_secs)
            .unwrap_or(DEFAULT_SUBAGENT_GIT_TIMEOUT_SECS);
        require_positive(
            "agent.subagents.git_timeout_secs",
            subagent_git_timeout_secs,
        )?;
        require_at_most(
            "agent.subagents.git_timeout_secs",
            subagent_git_timeout_secs,
            10 * 60,
        )?;
        let subagents = SubagentConfig {
            enabled: subagents_enabled,
            allow_mcp: file_subagents.allow_mcp.unwrap_or(false),
            worktree_dir: subagent_worktree_dir,
            max_parallel: subagent_max_parallel,
            max_per_session: subagent_max_per_session,
            max_tool_iterations: subagent_max_tool_iterations,
            max_tokens_per_agent: subagent_max_tokens_per_agent,
            max_total_tokens_per_session: subagent_max_total_tokens_per_session,
            max_depth: subagent_max_depth,
            max_children_per_agent: subagent_max_children,
            task_timeout: Duration::from_secs(subagent_task_timeout_secs),
            git_timeout: Duration::from_secs(subagent_git_timeout_secs),
        };

        let file_shell = file_agent.shell.unwrap_or_default();
        let shell_confirmation_mode = parse_shell_confirmation_mode(
            args.shell_confirmation_mode
                .or(file_shell.confirmation_mode)
                .as_deref(),
            file_ui.confirm_destructive,
        )?;
        let timeout_rule_values = args.shell_timeout_rules.unwrap_or_default();
        let timeout_rules = if timeout_rule_values.is_empty() {
            file_shell
                .timeout_rules
                .into_iter()
                .map(|rule| validate_shell_timeout_rule(rule.prefix, rule.timeout_secs))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            timeout_rule_values
                .into_iter()
                .map(|rule| parse_shell_timeout_rule(&rule))
                .collect::<Result<Vec<_>, _>>()?
        };
        let direct_exec_allowlist = if let Some(entries) = args.shell_direct_allowlist {
            entries
                .into_iter()
                .map(|entry| parse_shell_allowlist_entry(&entry))
                .collect::<Result<Vec<_>, _>>()?
        } else {
            file_shell
                .direct_exec_allowlist
                .into_iter()
                .map(|entry| validate_shell_allowlist_entry(entry.program, entry.args))
                .collect::<Result<Vec<_>, _>>()?
        };
        let terminal = build_interactive_terminal_config(file_shell.terminal.unwrap_or_default())?;
        let shell = ShellConfig {
            confirmation_mode: shell_confirmation_mode,
            timeout_rules,
            direct_exec_allowlist,
            terminal,
        };

        let file_whip = file_agent.whip.unwrap_or_default();
        let mut whip = WhipConfig::default();
        whip.enabled = args
            .whip_enabled
            .or(file_whip.enabled)
            .unwrap_or(whip.enabled);
        whip.hotkey = args
            .whip_hotkey
            .or(file_whip.hotkey)
            .unwrap_or_else(|| whip.hotkey.clone());
        whip.double_hit_window = Duration::from_millis(
            args.whip_double_hit_window_ms
                .or(file_whip.double_hit_window_ms)
                .unwrap_or(whip.double_hit_window.as_millis() as u64),
        );
        whip.penalty_completed_responses = args
            .whip_penalty_completed_responses
            .or(file_whip.penalty_completed_responses)
            .unwrap_or(whip.penalty_completed_responses);
        whip.max_output_percent = args
            .whip_max_output_percent
            .or(file_whip.max_output_percent)
            .unwrap_or(whip.max_output_percent);
        whip.minimum_output_tokens = args
            .whip_minimum_output_tokens
            .or(file_whip.minimum_output_tokens)
            .unwrap_or(whip.minimum_output_tokens);
        if whip.double_hit_window.is_zero()
            || whip.penalty_completed_responses == 0
            || whip.max_output_percent == 0
            || whip.max_output_percent > 100
            || whip.minimum_output_tokens == 0
        {
            return Err(ConfigError::InvalidValue {
                field: "agent.whip",
                message: "durations/counts must be positive and max_output_percent must be 1..=100"
                    .to_owned(),
            });
        }
        validate_whip_hotkey(&whip.hotkey)?;

        let log_level = args
            .log_level
            .or(file_log.level)
            .unwrap_or_else(|| "info".to_owned());
        let log_dir = args.log_dir.or(file_log.dir).unwrap_or_else(|| {
            directories::ProjectDirs::from("dev", "denysoid", "decode")
                .map(|dirs| dirs.data_local_dir().join("logs"))
                .unwrap_or_else(|| PathBuf::from("logs"))
        });
        let plugin_integration_root = skills
            .user_dir
            .parent()
            .unwrap_or(&skills.user_dir)
            .join("plugin-connections");
        merge_plugin_connections(&plugin_integration_root, &mut file_mcp, &mut file_lsp)?;
        let mcp = build_mcp_config(file_mcp)?;
        let lsp = build_lsp_config(file_lsp)?;
        let github = build_github_config(file_github)?;

        let api = ApiConfig {
            provider,
            auth,
            api_key,
            bedrock_runtime: BedrockRuntimeConfig {
                region: args.aws_region.or(file_api.aws_region),
                profile: args.aws_profile.or(file_api.aws_profile),
                role_arn: args.aws_role_arn.or(file_api.aws_role_arn),
                endpoint_url: args.bedrock_endpoint_url.or(file_api.bedrock_endpoint_url),
            },
            transport: api_transport,
            endpoint,
            allow_insecure_loopback,
            deployment: deployment.clone(),
            deployment_choices: deployment_choices.clone(),
            api_version,
            max_output_tokens,
            reasoning_effort,
            temperature,
            server_compaction_threshold,
            request_timeout: Duration::from_secs(request_timeout_secs),
            stream_idle_timeout: Duration::from_secs(stream_idle_timeout_secs),
            max_attempts,
            retry_min_delay,
            retry_max_delay,
            retry_after_cap,
            pricing,
            pricing_catalog_url,
        };
        api.validate()?;
        let code_index = build_code_index_config(file_code_index, &api)?;

        Ok(Self {
            api,
            deployment_choices,
            agent: AgentConfig {
                context_mode,
                context_budget,
                max_context_budget,
                max_tool_iterations,
                workspace_root,
                session_dir,
                privacy_user_rules_file,
                instructions_file,
                instructions,
                project_instructions,
                skills,
                exec_timeout: Duration::from_secs(exec_timeout_secs),
                subagents,
                shell,
                whip,
            },
            ui: UiConfig {
                confirm_destructive: file_ui.confirm_destructive.unwrap_or(true),
                mouse_enabled: file_ui.mouse_enabled.unwrap_or(true),
                language: UiLanguage::parse(file_ui.language.as_deref())?,
                onboarding_completed: file_ui.onboarding_completed.unwrap_or(false),
                mascot_enabled: file_ui.mascot_enabled.unwrap_or(true),
                show_thinking: file_ui.show_thinking.unwrap_or(true),
                show_tool_activity: file_ui.show_tool_activity.unwrap_or(true),
            },
            logging: LoggingConfig {
                level: log_level,
                dir: log_dir,
            },
            mcp,
            lsp,
            code_index,
            github,
        })
    }

    fn load_file_config(
        path: Option<&Path>,
        project_root_override: Option<&Path>,
    ) -> Result<FileConfig, ConfigError> {
        if let Some(path) = path {
            // A user-selected path is an assertion that this exact file should
            // be used; silently falling back would hide typos and stale setup.
            return read_file_config(path);
        }

        // The implicit source is deliberately only the per-user config. We do
        // not auto-load project-local config from an untrusted workspace.
        let mut config = if let Some(path) =
            directories::ProjectDirs::from("dev", "denysoid", "decode")
                .map(|dirs| dirs.config_dir().join("config.toml"))
                .filter(|path| path.exists())
        {
            read_file_config(&path)?
        } else {
            FileConfig::default()
        };

        let explicit_project_root = project_root_override.is_some();
        let mut project_root = project_root_override
            .map(Path::to_path_buf)
            .or_else(|| {
                config
                    .agent
                    .as_ref()
                    .and_then(|agent| agent.workspace_root.clone())
            })
            .unwrap_or(std::env::current_dir().map_err(ConfigError::CurrentDirectory)?);
        project_root = std::fs::canonicalize(&project_root).unwrap_or(project_root);
        if !explicit_project_root && let Some(root) = build_project_root(&project_root) {
            project_root = root;
        }
        let project_path = project_root.join(".decode.toml");
        if project_path.exists() {
            let project = read_project_file_config(&project_path)?;
            merge_project_config(&mut config, project);
        }
        Ok(config)
    }
}

fn build_pricing_catalog(
    provider: ApiProvider,
    entries: &[FileDeploymentPricing],
) -> Result<PricingCatalog, ConfigError> {
    let mut rates = entries
        .iter()
        .map(|entry| {
            let rate = DeploymentPricing::from_usd_per_million(
                entry.deployment.clone(),
                entry.input_usd_per_million,
                entry.cached_input_usd_per_million,
                entry.output_usd_per_million,
            )
            .map_err(|error| ConfigError::InvalidValue {
                field: "api.pricing",
                message: error.to_string(),
            })?;
            match (
                entry.long_context_threshold_tokens,
                entry.long_context_input_usd_per_million,
                entry.long_context_output_usd_per_million,
            ) {
                (None, None, None) if entry.long_context_cached_input_usd_per_million.is_none() => {
                    Ok(rate)
                }
                (Some(threshold), Some(input), Some(output)) => rate
                    .with_long_context_tier(
                        threshold,
                        input,
                        entry.long_context_cached_input_usd_per_million,
                        output,
                    )
                    .map_err(|error| ConfigError::InvalidValue {
                        field: "api.pricing",
                        message: error.to_string(),
                    }),
                _ => Err(ConfigError::InvalidValue {
                    field: "api.pricing",
                    message: "long-context pricing requires threshold, input, and output together"
                        .to_owned(),
                }),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let explicit = entries
        .iter()
        .map(|entry| entry.deployment.as_str())
        .collect::<HashSet<_>>();
    for &(deployment, input, cached, output, long) in verified_pricing_presets(provider) {
        if explicit.contains(deployment) {
            continue;
        }
        let mut rate = DeploymentPricing::from_usd_per_million(
            deployment.to_owned(),
            input,
            Some(cached),
            output,
        )
        .map_err(|error| ConfigError::InvalidValue {
            field: "api.pricing",
            message: error.to_string(),
        })?;
        if let Some((threshold, long_input, long_cached, long_output)) = long {
            rate = rate
                .with_long_context_tier(threshold, long_input, Some(long_cached), long_output)
                .map_err(|error| ConfigError::InvalidValue {
                    field: "api.pricing",
                    message: error.to_string(),
                })?;
        }
        let (source, label) = if provider == ApiProvider::Azure {
            (
                PricingSource::PublicCatalog,
                "OpenAI public reference; Azure billing may differ",
            )
        } else {
            (PricingSource::OfficialCatalog, "provider public pricing")
        };
        rate = rate.with_provenance(source, label, Some("2026-08-28".to_owned()));
        rates.push(rate);
    }
    PricingCatalog::new(rates).map_err(|error| ConfigError::InvalidValue {
        field: "api.pricing",
        message: error.to_string(),
    })
}

type VerifiedPricingPreset = (&'static str, f64, f64, f64, Option<(u64, f64, f64, f64)>);

/// Conservative built-ins are limited to exact, provider-owned model IDs
/// whose public standard token tariff has one unambiguous interpretation.
/// Region-, marketplace-, modality-, priority-, and time-dependent providers
/// intentionally require an explicit `[[api.pricing]]` entry.
fn verified_pricing_presets(provider: ApiProvider) -> &'static [VerifiedPricingPreset] {
    match provider {
        // OpenAI standard token tariffs, verified against the official API
        // pricing page on 2026-08-28. Azure deployment pricing can vary by
        // agreement/region, so exact user entries still override these
        // transparent estimates by deployment name.
        ApiProvider::Azure | ApiProvider::OpenAi => &[
            (
                "gpt-5.6-sol",
                4.0,
                0.4,
                20.0,
                Some((272_000, 8.0, 0.8, 30.0)),
            ),
            (
                "gpt-5.6-terra",
                2.0,
                0.2,
                12.0,
                Some((272_000, 4.0, 0.4, 18.0)),
            ),
            (
                "gpt-5.6-luna",
                0.2,
                0.02,
                1.2,
                Some((272_000, 0.4, 0.04, 1.8)),
            ),
            ("gpt-5.5", 5.0, 0.5, 30.0, Some((272_000, 10.0, 1.0, 45.0))),
            (
                "gpt-5.5-pro",
                30.0,
                30.0,
                180.0,
                Some((272_000, 60.0, 60.0, 270.0)),
            ),
            ("gpt-5.4", 2.5, 0.25, 15.0, Some((272_000, 5.0, 0.5, 22.5))),
            ("gpt-5.4-mini", 0.75, 0.075, 4.5, None),
            ("gpt-5.4-nano", 0.2, 0.02, 1.25, None),
            (
                "gpt-5.4-pro",
                30.0,
                30.0,
                180.0,
                Some((272_000, 60.0, 60.0, 270.0)),
            ),
            ("gpt-5.2", 1.75, 0.175, 14.0, None),
            ("gpt-5.2-pro", 21.0, 21.0, 168.0, None),
            ("gpt-5.1", 1.25, 0.125, 10.0, None),
            ("gpt-5", 1.25, 0.125, 10.0, None),
            ("gpt-5-mini", 0.25, 0.025, 2.0, None),
            ("gpt-5-nano", 0.05, 0.005, 0.4, None),
            ("gpt-5-pro", 15.0, 15.0, 120.0, None),
            ("gpt-4.1", 2.0, 0.5, 8.0, None),
            ("gpt-4.1-mini", 0.4, 0.1, 1.6, None),
            ("gpt-4.1-nano", 0.1, 0.025, 0.4, None),
            ("gpt-4o", 2.5, 1.25, 10.0, None),
            ("gpt-4o-mini", 0.15, 0.075, 0.6, None),
            ("o1", 15.0, 7.5, 60.0, None),
            ("o1-pro", 150.0, 150.0, 600.0, None),
            ("o3-pro", 20.0, 20.0, 80.0, None),
            ("o3", 2.0, 0.5, 8.0, None),
            ("o4-mini", 1.1, 0.275, 4.4, None),
            ("o3-mini", 1.1, 0.55, 4.4, None),
            ("gpt-3.5-turbo", 0.5, 0.5, 1.5, None),
            ("gpt-3.5-turbo-instruct", 1.5, 1.5, 2.0, None),
        ],
        ApiProvider::Google => &[
            (
                "gemini-3.1-pro-preview",
                2.0,
                0.2,
                12.0,
                Some((200_000, 4.0, 0.4, 18.0)),
            ),
            (
                "gemini-3.1-pro-preview-customtools",
                2.0,
                0.2,
                12.0,
                Some((200_000, 4.0, 0.4, 18.0)),
            ),
            ("gemini-3-flash-preview", 0.5, 0.05, 3.0, None),
        ],
        ApiProvider::Anthropic => &[
            ("claude-fable-5", 10.0, 1.0, 50.0, None),
            ("claude-mythos-5", 10.0, 1.0, 50.0, None),
            ("claude-opus-5", 5.0, 0.5, 25.0, None),
            ("claude-opus-4-8", 5.0, 0.5, 25.0, None),
            ("claude-opus-4-7", 5.0, 0.5, 25.0, None),
            ("claude-opus-4-6", 5.0, 0.5, 25.0, None),
            ("claude-opus-4-5", 5.0, 0.5, 25.0, None),
            ("claude-sonnet-5", 2.0, 0.2, 10.0, None),
            ("claude-sonnet-4-6", 3.0, 0.3, 15.0, None),
            ("claude-sonnet-4-5", 3.0, 0.3, 15.0, None),
            ("claude-haiku-4-5", 1.0, 0.1, 5.0, None),
        ],
        ApiProvider::AwsBedrock
        | ApiProvider::AwsBedrockRuntime
        | ApiProvider::OpenRouter
        | ApiProvider::XAi
        | ApiProvider::Groq
        | ApiProvider::Mistral
        | ApiProvider::DeepSeek
        | ApiProvider::Together
        | ApiProvider::Fireworks
        | ApiProvider::Cerebras
        | ApiProvider::Perplexity
        | ApiProvider::Nvidia
        | ApiProvider::SambaNova
        | ApiProvider::Moonshot
        | ApiProvider::Alibaba
        | ApiProvider::HuggingFace
        | ApiProvider::GitHubModels
        | ApiProvider::Ollama
        | ApiProvider::Compatible => &[],
    }
}

const MAX_PLUGIN_CONNECTION_FILES: usize = 64;
const MAX_PLUGIN_CONNECTION_FILE_BYTES: u64 = 256 * 1024;

fn merge_plugin_connections(
    root: &Path,
    mcp: &mut FileMcpConfig,
    lsp: &mut FileLspConfig,
) -> Result<(), ConfigError> {
    let namespaces = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(source) => {
            return Err(ConfigError::ConfigIo {
                path: root.to_path_buf(),
                source,
            });
        }
    };
    let mut namespaces = namespaces
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ConfigError::ConfigIo {
            path: root.to_path_buf(),
            source,
        })?;
    namespaces.sort_by_key(std::fs::DirEntry::file_name);
    let mut loaded = 0_usize;
    for namespace in namespaces {
        let metadata = std::fs::symlink_metadata(namespace.path()).map_err(|source| {
            ConfigError::ConfigIo {
                path: namespace.path(),
                source,
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ConfigError::InvalidValue {
                field: "plugins.connections",
                message: format!(
                    "plugin connection namespace must be a real directory: {}",
                    namespace.path().display()
                ),
            });
        }
        for kind in ["mcp", "lsp"] {
            for path in plugin_connection_files(&namespace.path().join(kind))? {
                loaded = loaded.saturating_add(1);
                if loaded > MAX_PLUGIN_CONNECTION_FILES {
                    return Err(ConfigError::InvalidValue {
                        field: "plugins.connections",
                        message: format!(
                            "enabled plugins provide more than {MAX_PLUGIN_CONNECTION_FILES} connection files"
                        ),
                    });
                }
                let text = read_plugin_connection_file(&path)?;
                if kind == "mcp" {
                    let contribution: PluginMcpContribution =
                        toml::from_str(&text).map_err(|source| ConfigError::Parse {
                            path: path.clone(),
                            message: format!(
                                "plugin MCP contribution may contain only [[servers]] entries: {source}"
                            ),
                        })?;
                    mcp.servers.extend(contribution.servers);
                } else {
                    let contribution: PluginLspContribution =
                        toml::from_str(&text).map_err(|source| ConfigError::Parse {
                            path: path.clone(),
                            message: format!(
                                "plugin LSP contribution may contain only [[servers]] entries: {source}"
                            ),
                        })?;
                    lsp.servers.extend(contribution.servers);
                }
            }
        }
    }
    Ok(())
}

fn plugin_connection_files(directory: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ConfigError::ConfigIo {
                path: directory.to_path_buf(),
                source,
            });
        }
    };
    let mut paths = entries
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ConfigError::ConfigIo {
            path: directory.to_path_buf(),
            source,
        })?
        .into_iter()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in &paths {
        let metadata = std::fs::symlink_metadata(path).map_err(|source| ConfigError::ConfigIo {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || path.extension().is_none_or(|extension| extension != "toml")
        {
            return Err(ConfigError::InvalidValue {
                field: "plugins.connections",
                message: format!(
                    "connection component must be a regular TOML file: {}",
                    path.display()
                ),
            });
        }
    }
    Ok(paths)
}

fn read_plugin_connection_file(path: &Path) -> Result<String, ConfigError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.len() > MAX_PLUGIN_CONNECTION_FILE_BYTES {
        return Err(ConfigError::InvalidValue {
            field: "plugins.connections",
            message: format!(
                "{} exceeds {MAX_PLUGIN_CONNECTION_FILE_BYTES} bytes",
                path.display()
            ),
        });
    }
    std::fs::read_to_string(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })
}

fn build_mcp_config(file: FileMcpConfig) -> Result<McpConfig, ConfigError> {
    let defaults = McpConfig::default();
    let mut servers = Vec::with_capacity(file.servers.len());
    for source in file.servers {
        let transport_name = source.transport.trim().to_ascii_lowercase();
        let transport = match transport_name.as_str() {
            "stdio" => {
                if source.url.is_some()
                    || source.bearer_token_env.is_some()
                    || !source.headers_from.is_empty()
                    || source.oauth.is_some()
                {
                    return Err(ConfigError::InvalidValue {
                        field: "mcp.servers",
                        message: format!(
                            "STDIO server {:?} cannot contain HTTP or OAuth fields",
                            source.name
                        ),
                    });
                }
                McpTransportConfig::Stdio {
                    command: source.command.ok_or_else(|| ConfigError::InvalidValue {
                        field: "mcp.servers.command",
                        message: format!("STDIO server {:?} requires command", source.name),
                    })?,
                    args: source.args,
                    env_from: source.env_from,
                    working_directory: source.working_directory,
                }
            }
            "http" | "streamable_http" => {
                if source.command.is_some()
                    || !source.args.is_empty()
                    || !source.env_from.is_empty()
                    || source.working_directory.is_some()
                {
                    return Err(ConfigError::InvalidValue {
                        field: "mcp.servers",
                        message: format!(
                            "HTTP server {:?} cannot contain STDIO process fields",
                            source.name
                        ),
                    });
                }
                McpTransportConfig::StreamableHttp {
                    url: source.url.ok_or_else(|| ConfigError::InvalidValue {
                        field: "mcp.servers.url",
                        message: format!("HTTP server {:?} requires url", source.name),
                    })?,
                    bearer_token_env: source.bearer_token_env,
                    headers_from: source.headers_from,
                    oauth: source.oauth.map(|oauth| McpOAuthConfig {
                        client_id: oauth.client_id,
                        scopes: oauth.scopes,
                        callback_port: oauth.callback_port.unwrap_or(0),
                    }),
                }
            }
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "mcp.servers.transport",
                    message: format!(
                        "expected 'stdio' or 'streamable_http' for server {:?}, got {other:?}",
                        source.name
                    ),
                });
            }
        };
        let approval = match source.approval.as_deref().unwrap_or("always") {
            "always" | "prompt" => McpApprovalMode::Always,
            "writes" => McpApprovalMode::Writes,
            "never" | "auto" => McpApprovalMode::Never,
            other => {
                return Err(ConfigError::InvalidValue {
                    field: "mcp.servers.approval",
                    message: format!(
                        "expected 'always', 'writes', or 'never' for server {:?}, got {other:?}",
                        source.name
                    ),
                });
            }
        };
        servers.push(McpServerConfig {
            name: source.name,
            enabled: source.enabled.unwrap_or(true),
            required: source.required.unwrap_or(false),
            transport,
            permissions: McpPermissionConfig {
                approval,
                enabled_tools: source.enabled_tools.into_iter().collect::<BTreeSet<_>>(),
                disabled_tools: source.disabled_tools.into_iter().collect::<BTreeSet<_>>(),
                trusted_read_only_tools: source
                    .trusted_read_only_tools
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            },
        });
    }
    let config = McpConfig {
        enabled: file.enabled.unwrap_or(!servers.is_empty()),
        startup_timeout: Duration::from_secs(
            file.startup_timeout_secs
                .unwrap_or(defaults.startup_timeout.as_secs()),
        ),
        tool_timeout: Duration::from_secs(
            file.tool_timeout_secs
                .unwrap_or(defaults.tool_timeout.as_secs()),
        ),
        max_result_bytes: file.max_result_bytes.unwrap_or(defaults.max_result_bytes),
        max_sse_event_bytes: file
            .max_sse_event_bytes
            .unwrap_or(defaults.max_sse_event_bytes),
        reconnect_max_attempts: file
            .reconnect_max_attempts
            .unwrap_or(defaults.reconnect_max_attempts),
        reconnect_base_delay: Duration::from_millis(
            file.reconnect_base_delay_ms
                .unwrap_or(defaults.reconnect_base_delay.as_millis() as u64),
        ),
        servers,
    };
    config.validate()?;
    Ok(config)
}

fn build_lsp_config(file: FileLspConfig) -> Result<LspConfig, ConfigError> {
    let defaults = LspConfig::default();
    let servers = file
        .servers
        .into_iter()
        .map(|server| LspServerConfig {
            name: server.name,
            enabled: server.enabled.unwrap_or(true),
            required: server.required.unwrap_or(false),
            auto_start: server.auto_start.unwrap_or(true),
            command: server.command,
            args: server.args,
            language_id: server.language_id,
            extensions: server.extensions,
            root_markers: server.root_markers,
        })
        .collect::<Vec<_>>();
    let config = LspConfig {
        enabled: file.enabled.unwrap_or(!servers.is_empty()),
        startup_timeout: Duration::from_secs(
            file.startup_timeout_secs
                .unwrap_or(defaults.startup_timeout.as_secs()),
        ),
        request_timeout: Duration::from_secs(
            file.request_timeout_secs
                .unwrap_or(defaults.request_timeout.as_secs()),
        ),
        max_message_bytes: file.max_message_bytes.unwrap_or(defaults.max_message_bytes),
        max_result_bytes: file.max_result_bytes.unwrap_or(defaults.max_result_bytes),
        max_diagnostics: file.max_diagnostics.unwrap_or(defaults.max_diagnostics),
        servers,
    };
    config.validate()?;
    Ok(config)
}

fn build_code_index_config(
    file: FileCodeIndexConfig,
    api: &ApiConfig,
) -> Result<CodeIndexConfig, ConfigError> {
    let defaults = CodeIndexConfig::default();
    let file_embeddings = file.embeddings.unwrap_or_default();
    let embedding_defaults = crate::code_index::EmbeddingConfig::default();
    let embeddings_enabled = file_embeddings.enabled.unwrap_or(false);
    let embedding_endpoint = if embeddings_enabled {
        file_embeddings
            .endpoint
            .map_or_else(|| embeddings_url_from_api(api), Ok)?
    } else {
        file_embeddings.endpoint.unwrap_or_default()
    };
    let config = CodeIndexConfig {
        enabled: file.enabled.unwrap_or(defaults.enabled),
        auto_refresh: file.auto_refresh.unwrap_or(defaults.auto_refresh),
        max_files: file.max_files.unwrap_or(defaults.max_files),
        max_file_bytes: file.max_file_bytes.unwrap_or(defaults.max_file_bytes),
        max_source_bytes: file.max_source_bytes.unwrap_or(defaults.max_source_bytes),
        max_chunks: file.max_chunks.unwrap_or(defaults.max_chunks),
        chunk_lines: file.chunk_lines.unwrap_or(defaults.chunk_lines),
        overlap_lines: file.overlap_lines.unwrap_or(defaults.overlap_lines),
        max_result_bytes: file.max_result_bytes.unwrap_or(defaults.max_result_bytes),
        embeddings: crate::code_index::EmbeddingConfig {
            enabled: embeddings_enabled,
            endpoint: embedding_endpoint,
            model: file_embeddings.model.unwrap_or_default(),
            provider: api.provider,
            auth: api.auth,
            api_key: api.api_key.clone(),
            api_version: api.api_version.clone(),
            dimensions: file_embeddings.dimensions,
            batch_size: file_embeddings
                .batch_size
                .unwrap_or(embedding_defaults.batch_size),
            max_chunks: file_embeddings
                .max_chunks
                .unwrap_or(embedding_defaults.max_chunks),
            max_input_bytes: file_embeddings
                .max_input_bytes
                .unwrap_or(embedding_defaults.max_input_bytes),
            request_timeout: Duration::from_secs(
                file_embeddings
                    .request_timeout_secs
                    .unwrap_or(embedding_defaults.request_timeout.as_secs()),
            ),
            max_attempts: file_embeddings.max_attempts.unwrap_or(api.max_attempts),
            hybrid_weight: file_embeddings
                .hybrid_weight
                .unwrap_or(embedding_defaults.hybrid_weight),
        },
    };
    config.validate()?;
    Ok(config)
}

fn embeddings_url_from_api(api: &ApiConfig) -> Result<String, ConfigError> {
    let responses = api.endpoint.resolved_url(api.allow_insecure_loopback)?;
    let mut url = url::Url::parse(&responses).map_err(|error| ConfigError::InvalidValue {
        field: "code_index.embeddings.endpoint",
        message: error.to_string(),
    })?;
    let path = url.path().trim_end_matches('/');
    let Some(prefix) = path.strip_suffix("/responses") else {
        return Err(ConfigError::InvalidValue {
            field: "code_index.embeddings.endpoint",
            message: "cannot derive /embeddings from api.responses_url; set code_index.embeddings.endpoint explicitly"
                .to_owned(),
        });
    };
    url.set_path(&format!("{prefix}/embeddings"));
    Ok(url.to_string())
}

fn build_github_config(file: FileGitHubConfig) -> Result<crate::github::GitHubConfig, ConfigError> {
    let defaults = crate::github::GitHubConfig::default();
    let config = crate::github::GitHubConfig {
        enabled: file.enabled.unwrap_or(defaults.enabled),
        program: file.program.unwrap_or(defaults.program),
        timeout: Duration::from_secs(file.timeout_secs.unwrap_or(defaults.timeout.as_secs())),
        max_pull_requests: file.max_pull_requests.unwrap_or(defaults.max_pull_requests),
    };
    config
        .validate()
        .map_err(|error| ConfigError::InvalidValue {
            field: "github",
            message: error.to_string(),
        })?;
    Ok(config)
}

fn read_file_config(path: &Path) -> Result<FileConfig, ConfigError> {
    let content = std::fs::read_to_string(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&content).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        message: source.to_string(),
    })
}

fn read_project_file_config(path: &Path) -> Result<ProjectFileConfig, ConfigError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ConfigError::InvalidValue {
            field: "project config",
            message: "must be a regular file, not a symbolic link or reparse point".to_owned(),
        });
    }
    if metadata.len() > MAX_PROJECT_CONFIG_BYTES {
        return Err(ConfigError::InvalidValue {
            field: "project config",
            message: format!("must not exceed {MAX_PROJECT_CONFIG_BYTES} bytes"),
        });
    }

    let file = std::fs::File::open(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = file.take(MAX_PROJECT_CONFIG_BYTES.saturating_add(1));
    let mut content = String::new();
    reader
        .read_to_string(&mut content)
        .map_err(|source| ConfigError::ConfigIo {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(content.len()).unwrap_or(u64::MAX) > MAX_PROJECT_CONFIG_BYTES {
        return Err(ConfigError::InvalidValue {
            field: "project config",
            message: format!("must not exceed {MAX_PROJECT_CONFIG_BYTES} bytes"),
        });
    }
    toml::from_str(&content).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        message: format!(
            "untrusted project config may contain only safe agent context/whip and ui.mouse_enabled fields: {source}"
        ),
    })
}

const fn provider_api_key_envs(provider: ApiProvider) -> &'static [&'static str] {
    match provider {
        ApiProvider::Azure => &["AZURE_OPENAI_API_KEY"],
        ApiProvider::OpenAi => &["OPENAI_API_KEY"],
        ApiProvider::Google => &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        ApiProvider::Anthropic => &["ANTHROPIC_API_KEY"],
        ApiProvider::AwsBedrock => &["AWS_BEARER_TOKEN_BEDROCK"],
        ApiProvider::AwsBedrockRuntime => &[],
        ApiProvider::OpenRouter => &["OPENROUTER_API_KEY"],
        ApiProvider::XAi => &["XAI_API_KEY"],
        ApiProvider::Groq => &["GROQ_API_KEY"],
        ApiProvider::Mistral => &["MISTRAL_API_KEY"],
        ApiProvider::DeepSeek => &["DEEPSEEK_API_KEY"],
        ApiProvider::Together => &["TOGETHER_API_KEY"],
        ApiProvider::Fireworks => &["FIREWORKS_API_KEY"],
        ApiProvider::Cerebras => &["CEREBRAS_API_KEY"],
        ApiProvider::Perplexity => &["PERPLEXITY_API_KEY", "PPLX_API_KEY"],
        ApiProvider::Nvidia => &["NVIDIA_API_KEY", "NVIDIA_NIM_API_KEY"],
        ApiProvider::SambaNova => &["SAMBANOVA_API_KEY"],
        ApiProvider::Moonshot => &["MOONSHOT_API_KEY"],
        ApiProvider::Alibaba => &["DASHSCOPE_API_KEY"],
        ApiProvider::HuggingFace => &["HF_TOKEN", "HUGGINGFACE_API_KEY"],
        ApiProvider::GitHubModels => &["GITHUB_TOKEN", "GH_TOKEN"],
        ApiProvider::Ollama => &["OLLAMA_API_KEY", "DECODE_PROVIDER_API_KEY"],
        ApiProvider::Compatible => &["DECODE_PROVIDER_API_KEY"],
    }
}

const fn default_provider_endpoint(provider: ApiProvider) -> Option<&'static str> {
    match provider {
        ApiProvider::Google => Some("https://generativelanguage.googleapis.com/v1beta/models"),
        ApiProvider::Anthropic => Some("https://api.anthropic.com/v1/messages"),
        ApiProvider::OpenRouter => Some("https://openrouter.ai/api/v1/chat/completions"),
        ApiProvider::XAi => Some("https://api.x.ai/v1/chat/completions"),
        ApiProvider::Groq => Some("https://api.groq.com/openai/v1/chat/completions"),
        ApiProvider::Mistral => Some("https://api.mistral.ai/v1/chat/completions"),
        ApiProvider::DeepSeek => Some("https://api.deepseek.com/chat/completions"),
        ApiProvider::Together => Some("https://api.together.xyz/v1/chat/completions"),
        ApiProvider::Fireworks => Some("https://api.fireworks.ai/inference/v1/chat/completions"),
        ApiProvider::Cerebras => Some("https://api.cerebras.ai/v1/chat/completions"),
        ApiProvider::Perplexity => Some("https://api.perplexity.ai/chat/completions"),
        ApiProvider::Nvidia => Some("https://integrate.api.nvidia.com/v1/chat/completions"),
        ApiProvider::SambaNova => Some("https://api.sambanova.ai/v1/chat/completions"),
        ApiProvider::Moonshot => Some("https://api.moonshot.ai/v1/chat/completions"),
        ApiProvider::Alibaba => {
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions")
        }
        ApiProvider::HuggingFace => Some("https://router.huggingface.co/v1/chat/completions"),
        ApiProvider::GitHubModels => Some("https://models.github.ai/inference/chat/completions"),
        ApiProvider::Ollama => Some("http://127.0.0.1:11434/v1/chat/completions"),
        ApiProvider::Azure
        | ApiProvider::OpenAi
        | ApiProvider::AwsBedrock
        | ApiProvider::AwsBedrockRuntime
        | ApiProvider::Compatible => None,
    }
}

fn read_provider_keyring(account: &str) -> Result<Option<String>, ConfigError> {
    if account.is_empty()
        || account.len() > 128
        || account.chars().any(|character| character.is_control())
    {
        return Err(ConfigError::InvalidValue {
            field: "api.keyring_account",
            message: "must be a visible identifier of at most 128 bytes".to_owned(),
        });
    }
    let entry = keyring::Entry::new("decode-provider", account)
        .map_err(|error| ConfigError::CredentialStore(error.to_string()))?;
    match entry.get_password() {
        Ok(secret) if !secret.is_empty() => Ok(Some(secret)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(ConfigError::CredentialStore(error.to_string())),
    }
}

fn read_explicit_env_key(
    path: &Path,
    workspace_root: &Path,
    provider: ApiProvider,
) -> Result<String, ConfigError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| ConfigError::ConfigIo {
        path: path.to_path_buf(),
        source,
    })?;
    if canonical.starts_with(workspace_root) {
        return Err(ConfigError::InvalidValue {
            field: "env_file",
            message: "credential files must be outside the canonical workspace".to_owned(),
        });
    }
    if !canonical.is_file() {
        return Err(ConfigError::InvalidValue {
            field: "env_file",
            message: "credential path is not a regular file".to_owned(),
        });
    }

    let entries = dotenvy::from_path_iter(&canonical).map_err(|_| ConfigError::InvalidValue {
        field: "env_file",
        message: "failed to parse the explicitly selected credential file".to_owned(),
    })?;
    let mut api_key = None;
    for entry in entries {
        let (name, value) = entry.map_err(|_| ConfigError::InvalidValue {
            field: "env_file",
            message: "failed to parse the explicitly selected credential file".to_owned(),
        })?;
        if provider_api_key_envs(provider).contains(&name.as_str()) {
            api_key = Some(value);
        }
    }
    api_key
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::MissingApiKey)
}

fn merge_project_config(base: &mut FileConfig, project: ProjectFileConfig) {
    if let Some(project_agent) = project.agent {
        let agent = base.agent.get_or_insert_with(FileAgentConfig::default);
        if project_agent.context_mode.is_some() {
            agent.context_mode = project_agent.context_mode;
        }
        if project_agent.context_budget.is_some() {
            agent.context_budget = project_agent.context_budget;
        }
        if project_agent.max_tool_iterations.is_some() {
            agent.max_tool_iterations = project_agent.max_tool_iterations;
        }
        if let Some(project_whip) = project_agent.whip {
            let whip = agent.whip.get_or_insert_with(FileWhipConfig::default);
            if project_whip.enabled.is_some() {
                whip.enabled = project_whip.enabled;
            }
            if project_whip.hotkey.is_some() {
                whip.hotkey = project_whip.hotkey;
            }
            if project_whip.double_hit_window_ms.is_some() {
                whip.double_hit_window_ms = project_whip.double_hit_window_ms;
            }
            if project_whip.penalty_completed_responses.is_some() {
                whip.penalty_completed_responses = project_whip.penalty_completed_responses;
            }
            if project_whip.max_output_percent.is_some() {
                whip.max_output_percent = project_whip.max_output_percent;
            }
            if project_whip.minimum_output_tokens.is_some() {
                whip.minimum_output_tokens = project_whip.minimum_output_tokens;
            }
        }
    }
    if let Some(project_ui) = project.ui {
        let ui = base.ui.get_or_insert_with(FileUiConfig::default);
        if project_ui.mouse_enabled.is_some() {
            ui.mouse_enabled = project_ui.mouse_enabled;
        }
    }
}

fn validate_project_instructions(config: &ProjectInstructionsConfig) -> Result<(), ConfigError> {
    if config.max_source_bytes == 0 {
        return Err(ConfigError::InvalidValue {
            field: "agent.project_instructions.max_source_bytes",
            message: "must be greater than zero".to_owned(),
        });
    }
    if config.max_total_bytes < config.max_source_bytes
        || config.max_total_bytes > MAX_PROJECT_INSTRUCTION_TOTAL_BYTES
    {
        return Err(ConfigError::InvalidValue {
            field: "agent.project_instructions.max_total_bytes",
            message: format!(
                "must be between max_source_bytes and {MAX_PROJECT_INSTRUCTION_TOTAL_BYTES}"
            ),
        });
    }
    if !(1..=MAX_PROJECT_INSTRUCTION_SOURCES).contains(&config.max_sources) {
        return Err(ConfigError::InvalidValue {
            field: "agent.project_instructions.max_sources",
            message: format!("must be between 1 and {MAX_PROJECT_INSTRUCTION_SOURCES}"),
        });
    }
    if config.max_include_depth > MAX_PROJECT_INSTRUCTION_INCLUDE_DEPTH {
        return Err(ConfigError::InvalidValue {
            field: "agent.project_instructions.max_include_depth",
            message: format!("must not exceed {MAX_PROJECT_INSTRUCTION_INCLUDE_DEPTH}"),
        });
    }
    Ok(())
}

fn validate_skills(config: &SkillsConfig) -> Result<(), ConfigError> {
    if !(1_024..=MAX_SKILLS_METADATA_BUDGET_BYTES).contains(&config.metadata_budget_bytes) {
        return Err(ConfigError::InvalidValue {
            field: "agent.skills.metadata_budget_bytes",
            message: format!("must be between 1024 and {MAX_SKILLS_METADATA_BUDGET_BYTES}"),
        });
    }
    if !(1..=MAX_SKILLS).contains(&config.max_skills) {
        return Err(ConfigError::InvalidValue {
            field: "agent.skills.max_skills",
            message: format!("must be between 1 and {MAX_SKILLS}"),
        });
    }
    if !(1_024..=MAX_SKILL_BYTES).contains(&config.max_skill_bytes) {
        return Err(ConfigError::InvalidValue {
            field: "agent.skills.max_skill_bytes",
            message: format!("must be between 1024 and {MAX_SKILL_BYTES}"),
        });
    }
    if !(1_024..=MAX_SKILL_RESOURCE_BYTES).contains(&config.max_resource_bytes) {
        return Err(ConfigError::InvalidValue {
            field: "agent.skills.max_resource_bytes",
            message: format!("must be between 1024 and {MAX_SKILL_RESOURCE_BYTES}"),
        });
    }
    if !(1..=MAX_SKILL_RESOURCES).contains(&config.max_resources) {
        return Err(ConfigError::InvalidValue {
            field: "agent.skills.max_resources",
            message: format!("must be between 1 and {MAX_SKILL_RESOURCES}"),
        });
    }
    Ok(())
}

fn non_empty(field: &'static str, value: String) -> Result<String, ConfigError> {
    non_empty_ref(field, &value)?;
    Ok(value)
}

fn non_empty_ref(field: &'static str, value: &str) -> Result<(), ConfigError> {
    if value.trim().is_empty() {
        return Err(ConfigError::InvalidValue {
            field,
            message: "must not be empty".to_owned(),
        });
    }
    Ok(())
}

fn require_positive<T>(field: &'static str, value: T) -> Result<(), ConfigError>
where
    T: PartialEq + From<u8>,
{
    if value == T::from(0) {
        return Err(ConfigError::InvalidValue {
            field,
            message: "must be greater than zero".to_owned(),
        });
    }
    Ok(())
}

fn require_at_most(field: &'static str, value: u64, maximum: u64) -> Result<(), ConfigError> {
    if value > maximum {
        return Err(ConfigError::InvalidValue {
            field,
            message: format!("must not exceed {maximum}"),
        });
    }
    Ok(())
}

fn validate_timeout(field: &'static str, value: Duration) -> Result<(), ConfigError> {
    if value.is_zero() || value > Duration::from_secs(MAX_TIMEOUT_SECS) {
        return Err(ConfigError::InvalidValue {
            field,
            message: format!("must be greater than zero and no more than {MAX_TIMEOUT_SECS}s"),
        });
    }
    Ok(())
}

fn validate_api_url(
    field: &'static str,
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<reqwest::Url, ConfigError> {
    let url = reqwest::Url::parse(value).map_err(|source| ConfigError::InvalidValue {
        field,
        message: source.to_string(),
    })?;
    if url.cannot_be_a_base()
        || url.host_str().is_none()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(ConfigError::InvalidValue {
            field,
            message: "must be an absolute URL without credentials or a fragment".to_owned(),
        });
    }

    match url.scheme() {
        "https" => {}
        "http" if allow_insecure_loopback && is_loopback_url(&url) => {}
        "http" => {
            return Err(ConfigError::InvalidValue {
                field,
                message:
                    "HTTP is allowed only for loopback hosts with allow_insecure_loopback=true"
                        .to_owned(),
            });
        }
        _ => {
            return Err(ConfigError::InvalidValue {
                field,
                message: "scheme must be HTTPS".to_owned(),
            });
        }
    }
    Ok(url)
}

fn is_loopback_url(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    if host.trim_end_matches('.').eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn responses_url_from_base(
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<String, ConfigError> {
    non_empty_ref("api.azure_base_url", value)?;
    let mut url = validate_api_url("api.azure_base_url", value, allow_insecure_loopback)?;
    if url.query().is_some() {
        return Err(ConfigError::InvalidValue {
            field: "api.azure_base_url",
            message: "must not contain a query; use api.responses_url for a full URL".to_owned(),
        });
    }
    url.path_segments_mut()
        .map_err(|()| ConfigError::InvalidValue {
            field: "api.azure_base_url",
            message: "cannot be used as a hierarchical base URL".to_owned(),
        })?
        .pop_if_empty()
        .push("responses");
    Ok(url.into())
}

fn endpoint_from_stored_responses_url(provider: ApiProvider, value: String) -> ResponsesEndpoint {
    let is_legacy_azure_base = provider == ApiProvider::Azure
        && url::Url::parse(&value).is_ok_and(|url| {
            url.query().is_none()
                && url.fragment().is_none()
                && url
                    .path()
                    .trim_end_matches('/')
                    .eq_ignore_ascii_case("/openai/v1")
        });
    if is_legacy_azure_base {
        ResponsesEndpoint::AzureBaseUrl(value)
    } else {
        ResponsesEndpoint::FullUrl(value)
    }
}

pub(crate) fn build_project_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().skip(1).find_map(|ancestor| {
        if ancestor.as_os_str().is_empty() {
            return None;
        }
        let relative = path.strip_prefix(ancestor).ok()?;
        let first = relative.components().next()?.as_os_str().to_string_lossy();
        (matches!(first.as_ref(), "target" | "dist") && ancestor.join("Cargo.toml").is_file())
            .then(|| ancestor.to_path_buf())
    })
}

fn instructions_too_large(actual: u64) -> ConfigError {
    ConfigError::InvalidValue {
        field: "agent.instructions_file",
        message: format!("must not exceed {MAX_INSTRUCTIONS_BYTES} bytes (found {actual})"),
    }
}

fn read_instructions_bounded(path: &Path) -> Result<String, ConfigError> {
    let file = std::fs::File::open(path).map_err(|source| ConfigError::InstructionsIo {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = file.take(MAX_INSTRUCTIONS_BYTES.saturating_add(1));
    let mut instructions = String::new();
    reader
        .read_to_string(&mut instructions)
        .map_err(|source| ConfigError::InstructionsIo {
            path: path.to_path_buf(),
            source,
        })?;
    let actual = u64::try_from(instructions.len()).unwrap_or(u64::MAX);
    if actual > MAX_INSTRUCTIONS_BYTES {
        return Err(instructions_too_large(actual));
    }
    Ok(instructions)
}

fn canonical_directory(field: &'static str, path: &Path) -> Result<PathBuf, ConfigError> {
    let canonical = std::fs::canonicalize(path).map_err(|source| ConfigError::PathIo {
        field,
        path: path.to_path_buf(),
        source,
    })?;
    if !canonical.is_dir() {
        return Err(ConfigError::NotDirectory {
            field,
            path: canonical,
        });
    }
    Ok(canonical)
}

fn parse_shell_confirmation_mode(
    value: Option<&str>,
    deprecated_confirm_destructive: Option<bool>,
) -> Result<ShellConfirmationMode, ConfigError> {
    let value = value.map(str::trim).filter(|value| !value.is_empty());
    match value {
        Some("always" | "always_confirm") => Ok(ShellConfirmationMode::Always),
        Some("strict" | "strict_allowlist") => Ok(ShellConfirmationMode::StrictAllowlist),
        Some(other) => Err(ConfigError::InvalidValue {
            field: "agent.shell.confirmation_mode",
            message: format!("expected 'always_confirm' or 'strict_allowlist', got {other:?}"),
        }),
        None if deprecated_confirm_destructive == Some(false) => {
            Ok(ShellConfirmationMode::StrictAllowlist)
        }
        None => Ok(ShellConfirmationMode::Always),
    }
}

fn parse_shell_timeout_rule(value: &str) -> Result<ShellTimeoutRule, ConfigError> {
    let Some((prefix, seconds)) = value.rsplit_once('=') else {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.timeout_rules",
            message: format!("expected PREFIX=SECONDS, got {value:?}"),
        });
    };
    let timeout_secs =
        seconds
            .trim()
            .parse::<u64>()
            .map_err(|error| ConfigError::InvalidValue {
                field: "agent.shell.timeout_rules",
                message: format!("invalid timeout in {value:?}: {error}"),
            })?;
    validate_shell_timeout_rule(prefix.to_owned(), timeout_secs)
}

fn validate_shell_timeout_rule(
    prefix: String,
    timeout_secs: u64,
) -> Result<ShellTimeoutRule, ConfigError> {
    let prefix = prefix.trim().to_owned();
    if prefix.is_empty() {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.timeout_rules",
            message: "command prefix must not be empty".to_owned(),
        });
    }
    require_positive("agent.shell.timeout_rules.timeout_secs", timeout_secs)?;
    require_at_most(
        "agent.shell.timeout_rules.timeout_secs",
        timeout_secs,
        MAX_TIMEOUT_SECS,
    )?;
    Ok(ShellTimeoutRule {
        prefix,
        timeout: Duration::from_secs(timeout_secs),
    })
}

fn parse_shell_allowlist_entry(value: &str) -> Result<StrictAllowlistEntry, ConfigError> {
    let mut fields = value.split('|');
    let program = fields.next().unwrap_or_default().to_owned();
    let args = fields.map(str::to_owned).collect::<Vec<_>>();
    validate_shell_allowlist_entry(program, args)
}

fn validate_shell_allowlist_entry(
    program: String,
    args: Vec<String>,
) -> Result<StrictAllowlistEntry, ConfigError> {
    StrictAllowlistEntry::new(program, args).map_err(|error| ConfigError::InvalidValue {
        field: "agent.shell.direct_exec_allowlist",
        message: error.to_string(),
    })
}

fn build_interactive_terminal_config(
    file: FileInteractiveTerminalConfig,
) -> Result<InteractiveTerminalConfig, ConfigError> {
    let defaults = InteractiveTerminalConfig::default();
    let program = file
        .program
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if program.as_ref().is_some_and(|value| value.contains('\0')) {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.terminal.program",
            message: "must not contain NUL bytes".to_owned(),
        });
    }
    if file.args.len() > MAX_TERMINAL_ARGUMENTS {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.terminal.args",
            message: format!("must contain at most {MAX_TERMINAL_ARGUMENTS} arguments"),
        });
    }
    if file
        .args
        .iter()
        .any(|argument| argument.contains('\0') || argument.len() > MAX_TERMINAL_ARGUMENT_BYTES)
    {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.terminal.args",
            message: format!(
                "arguments must not contain NUL bytes or exceed {MAX_TERMINAL_ARGUMENT_BYTES} bytes"
            ),
        });
    }
    let max_sessions = file.max_sessions.unwrap_or(defaults.max_sessions);
    if !(1..=MAX_TERMINAL_SESSIONS).contains(&max_sessions) {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.terminal.max_sessions",
            message: format!("must be between 1 and {MAX_TERMINAL_SESSIONS}"),
        });
    }
    let scrollback_lines = file.scrollback_lines.unwrap_or(defaults.scrollback_lines);
    if !(100..=MAX_TERMINAL_SCROLLBACK_LINES).contains(&scrollback_lines) {
        return Err(ConfigError::InvalidValue {
            field: "agent.shell.terminal.scrollback_lines",
            message: format!("must be between 100 and {MAX_TERMINAL_SCROLLBACK_LINES}"),
        });
    }
    Ok(InteractiveTerminalConfig {
        enabled: file.enabled.unwrap_or(defaults.enabled),
        program,
        args: file.args,
        max_sessions,
        scrollback_lines,
    })
}

fn validate_whip_hotkey(value: &str) -> Result<(), ConfigError> {
    let mut characters = value.chars();
    let Some(character) = characters.next() else {
        return Err(ConfigError::InvalidValue {
            field: "agent.whip.hotkey",
            message: "must contain exactly one printable character".to_owned(),
        });
    };
    if characters.next().is_some() || character.is_control() {
        return Err(ConfigError::InvalidValue {
            field: "agent.whip.hotkey",
            message: "must contain exactly one printable character".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use secrecy::SecretString;

    use super::{
        ApiAuth, ApiConfig, ApiProvider, AppConfig, CliArgs, DEFAULT_EXEC_TIMEOUT_SECS,
        DEFAULT_MAX_TOOL_ITERATIONS, FileConfig, FileDeploymentPricing,
        FileInteractiveTerminalConfig, FileLspConfig, FileMcpConfig, MAX_RETRY_AFTER_SECS,
        MAX_TIMEOUT_SECS, ProjectInstructionsConfig, ResponsesEndpoint, SkillsConfig,
        build_code_index_config, build_interactive_terminal_config, build_lsp_config,
        build_mcp_config, build_pricing_catalog, merge_plugin_connections,
        parse_shell_allowlist_entry, responses_url_from_base, validate_api_url,
        validate_project_instructions, validate_skills,
    };
    use crate::api::ReasoningEffort;
    use crate::error::ConfigError;
    use crate::usage::UsageLedger;

    fn valid_api_config() -> ApiConfig {
        ApiConfig {
            provider: ApiProvider::Azure,
            auth: ApiAuth::ApiKey,
            api_key: SecretString::new("secret".to_owned().into()),
            bedrock_runtime: super::BedrockRuntimeConfig::default(),
            transport: super::ApiTransport::Sse,
            endpoint: ResponsesEndpoint::FullUrl(
                "https://example.test/openai/v1/responses".to_owned(),
            ),
            allow_insecure_loopback: false,
            deployment: "model".to_owned(),
            deployment_choices: vec!["model".to_owned()],
            api_version: None,
            max_output_tokens: 128,
            reasoning_effort: ReasoningEffort::Medium,
            temperature: None,
            server_compaction_threshold: None,
            request_timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(10),
            max_attempts: 3,
            retry_min_delay: Duration::from_millis(10),
            retry_max_delay: Duration::from_secs(1),
            retry_after_cap: Duration::from_secs(MAX_RETRY_AFTER_SECS),
            pricing: crate::usage::PricingCatalog::default(),
            pricing_catalog_url: None,
        }
    }

    fn load_with_agent_limits(
        max_tool_iterations: u32,
        exec_timeout_secs: u64,
    ) -> Result<AppConfig, ConfigError> {
        let root = tempfile::tempdir().map_err(|source| ConfigError::ConfigIo {
            path: std::env::temp_dir(),
            source,
        })?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).map_err(|source| ConfigError::ConfigIo {
            path: workspace.clone(),
            source,
        })?;
        let instructions = workspace.join("instructions.md");
        std::fs::write(&instructions, "system\n").map_err(|source| ConfigError::ConfigIo {
            path: instructions.clone(),
            source,
        })?;
        let credentials = root.path().join("credentials.env");
        std::fs::write(&credentials, "AZURE_OPENAI_API_KEY=test-key\n").map_err(|source| {
            ConfigError::ConfigIo {
                path: credentials.clone(),
                source,
            }
        })?;
        let config_file = root.path().join("config.toml");
        let skills = root.path().join("skills");
        std::fs::write(
            &config_file,
            format!(
                concat!(
                    "[api]\n",
                    "responses_url = 'https://example.test/v1/responses'\n",
                    "deployment = 'model'\n",
                    "[agent]\n",
                    "max_tool_iterations = {}\n",
                    "exec_timeout_secs = {}\n",
                    "[agent.skills]\n",
                    "user_dir = '{}'\n"
                ),
                max_tool_iterations,
                exec_timeout_secs,
                skills.display()
            ),
        )
        .map_err(|source| ConfigError::ConfigIo {
            path: config_file.clone(),
            source,
        })?;
        AppConfig::load_from(CliArgs {
            config_file: Some(config_file),
            env_file: Some(credentials),
            workspace: Some(workspace),
            instructions_file: Some(instructions),
            ..CliArgs::default()
        })
    }

    #[test]
    fn base_url_appends_responses_without_replacing_the_last_segment() -> Result<(), ConfigError> {
        assert_eq!(
            responses_url_from_base("https://example.test/openai/v1", false)?,
            "https://example.test/openai/v1/responses"
        );
        assert_eq!(
            responses_url_from_base("https://example.test/openai/v1/", false)?,
            "https://example.test/openai/v1/responses"
        );
        Ok(())
    }

    #[test]
    fn legacy_onboarding_azure_base_in_responses_url_is_repaired()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let instructions = root.path().join("instructions.md");
        std::fs::write(&instructions, "system\n")?;
        let credentials = root.path().join("credentials.env");
        std::fs::write(&credentials, "AZURE_OPENAI_API_KEY=test-key\n")?;
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                concat!(
                    "[api]\n",
                    "provider = 'azure'\n",
                    "responses_url = 'https://resource.example/openai/v1'\n",
                    "deployment = 'deployment'\n",
                    "[agent]\n",
                    "workspace_root = {:?}\n",
                    "instructions_file = {:?}\n"
                ),
                workspace.to_string_lossy(),
                instructions.to_string_lossy(),
            ),
        )?;

        let config = AppConfig::load_from(CliArgs {
            config_file: Some(config_file),
            env_file: Some(credentials),
            ..CliArgs::default()
        })?;

        assert_eq!(
            config.api.endpoint.resolved_url(false)?,
            "https://resource.example/openai/v1/responses"
        );
        Ok(())
    }

    #[test]
    fn legacy_build_directory_workspace_is_promoted_to_project_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let project = root.path().join("project");
        let build_workspace = project.join("target").join("release");
        std::fs::create_dir_all(&build_workspace)?;
        std::fs::write(project.join("Cargo.toml"), "[package]\nname = 'fixture'\n")?;
        let instructions = root.path().join("instructions.md");
        std::fs::write(&instructions, "system\n")?;
        let config_file = root.path().join("config.toml");
        std::fs::write(
            &config_file,
            format!(
                concat!(
                    "[api]\n",
                    "provider = 'ollama'\n",
                    "deployment = 'qwen3-coder'\n",
                    "[agent]\n",
                    "workspace_root = {:?}\n",
                    "instructions_file = {:?}\n"
                ),
                build_workspace.to_string_lossy(),
                instructions.to_string_lossy(),
            ),
        )?;

        let config = AppConfig::load_from(CliArgs {
            config_file: Some(config_file.clone()),
            ..CliArgs::default()
        })?;

        assert_eq!(config.agent.workspace_root, std::fs::canonicalize(project)?);

        let explicit = AppConfig::load_from(CliArgs {
            config_file: Some(config_file),
            workspace: Some(build_workspace.clone()),
            ..CliArgs::default()
        })?;
        assert_eq!(
            explicit.agent.workspace_root,
            std::fs::canonicalize(build_workspace)?
        );
        Ok(())
    }

    #[test]
    fn plaintext_requires_an_explicit_loopback_opt_in() {
        assert!(validate_api_url("url", "http://127.0.0.1:8080/responses", false).is_err());
        assert!(validate_api_url("url", "http://example.test/responses", true).is_err());
        assert!(validate_api_url("url", "http://[::1]:8080/responses", true).is_ok());
    }

    #[test]
    fn direct_public_api_config_is_validated() {
        let mut config = valid_api_config();
        config.max_attempts = 0;
        assert!(config.validate().is_err());
        config.max_attempts = 1;
        config.request_timeout = Duration::ZERO;
        assert!(config.validate().is_err());
    }

    #[test]
    fn agent_iteration_and_execution_limits_are_bounded() {
        assert!(matches!(
            load_with_agent_limits(101, DEFAULT_EXEC_TIMEOUT_SECS),
            Err(ConfigError::InvalidValue {
                field: "agent.max_tool_iterations",
                ..
            })
        ));
        assert!(matches!(
            load_with_agent_limits(DEFAULT_MAX_TOOL_ITERATIONS, MAX_TIMEOUT_SECS + 1),
            Err(ConfigError::InvalidValue {
                field: "agent.exec_timeout_secs",
                ..
            })
        ));
    }

    #[test]
    fn provider_validation_is_fail_closed_and_azure_first() {
        assert!(matches!(ApiProvider::parse(None), Ok(ApiProvider::Azure)));
        assert!(matches!(
            ApiAuth::parse(None, ApiProvider::Azure),
            Ok(ApiAuth::ApiKey)
        ));
        assert!(matches!(
            ApiAuth::parse(None, ApiProvider::OpenAi),
            Ok(ApiAuth::Bearer)
        ));

        let mut config = valid_api_config();
        config.provider = ApiProvider::OpenAi;
        config.auth = ApiAuth::Bearer;
        config.endpoint = ResponsesEndpoint::OpenAi;
        assert!(config.validate().is_ok());

        config.auth = ApiAuth::ApiKey;
        assert!(config.validate().is_err());
        config.auth = ApiAuth::Bearer;
        config.api_version = Some("preview".to_owned());
        assert!(config.validate().is_err());

        config.provider = ApiProvider::Compatible;
        config.endpoint = ResponsesEndpoint::OpenAi;
        config.api_version = None;
        assert!(config.validate().is_err());

        config.provider = ApiProvider::Azure;
        config.auth = ApiAuth::ApiKey;
        assert!(config.validate().is_err());
    }

    #[test]
    fn bedrock_aliases_keep_mantle_legacy_and_select_native_runtime_explicitly() {
        assert!(matches!(
            ApiProvider::parse(Some("bedrock")),
            Ok(ApiProvider::AwsBedrock)
        ));
        assert!(matches!(
            ApiProvider::parse(Some("bedrock_mantle")),
            Ok(ApiProvider::AwsBedrock)
        ));
        assert!(matches!(
            ApiProvider::parse(Some("bedrock_runtime")),
            Ok(ApiProvider::AwsBedrockRuntime)
        ));
        assert!(matches!(
            ApiAuth::parse(None, ApiProvider::AwsBedrockRuntime),
            Ok(ApiAuth::AwsSdk)
        ));

        let mut config = valid_api_config();
        config.provider = ApiProvider::AwsBedrockRuntime;
        config.auth = ApiAuth::AwsSdk;
        config.api_key = SecretString::new(String::new().into());
        config.endpoint = ResponsesEndpoint::AwsBedrockRuntime;
        assert!(config.validate().is_ok());

        config.auth = ApiAuth::Bearer;
        assert!(config.validate().is_err());
    }

    #[test]
    fn websocket_transport_is_explicit_and_openai_only() {
        assert!(matches!(
            super::ApiTransport::parse(Some("websocket")),
            Ok(super::ApiTransport::WebSocket)
        ));
        assert_eq!(
            super::ApiTransport::Auto.resolved(ApiProvider::Azure),
            super::ApiTransport::Sse
        );
        assert_eq!(
            super::ApiTransport::Auto.resolved(ApiProvider::OpenAi),
            super::ApiTransport::WebSocket
        );

        let mut config = valid_api_config();
        config.transport = super::ApiTransport::WebSocket;
        assert!(config.validate().is_err());
        config.provider = ApiProvider::OpenAi;
        config.auth = ApiAuth::Bearer;
        config.endpoint = ResponsesEndpoint::OpenAi;
        config.api_version = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn direct_allowlist_cli_entries_preserve_exact_argv() -> Result<(), ConfigError> {
        #[cfg(unix)]
        let (encoded, program, argument) = ("uname|-n", "uname", "-n");
        #[cfg(windows)]
        let (encoded, program, argument) = ("whoami|/user", "whoami", "/user");
        let entry = parse_shell_allowlist_entry(encoded)?;
        assert_eq!(entry.program(), program);
        assert_eq!(entry.args(), &[argument]);
        assert!(parse_shell_allowlist_entry("cargo|check").is_err());
        assert!(parse_shell_allowlist_entry("cmd|/c|whoami").is_err());
        Ok(())
    }

    #[test]
    fn project_instruction_limits_are_bounded_and_consistent() {
        assert!(validate_project_instructions(&ProjectInstructionsConfig::default()).is_ok());
        let invalid_total = ProjectInstructionsConfig {
            max_source_bytes: 2_048,
            max_total_bytes: 1_024,
            ..ProjectInstructionsConfig::default()
        };
        assert!(validate_project_instructions(&invalid_total).is_err());
        let invalid_sources = ProjectInstructionsConfig {
            max_sources: 0,
            ..ProjectInstructionsConfig::default()
        };
        assert!(validate_project_instructions(&invalid_sources).is_err());
        let invalid_depth = ProjectInstructionsConfig {
            max_include_depth: 17,
            ..ProjectInstructionsConfig::default()
        };
        assert!(validate_project_instructions(&invalid_depth).is_err());
    }

    #[test]
    fn skill_limits_are_absolute_and_bounded() {
        assert!(validate_skills(&SkillsConfig::default()).is_ok());
        let too_small_metadata = SkillsConfig {
            metadata_budget_bytes: 1_023,
            ..SkillsConfig::default()
        };
        assert!(validate_skills(&too_small_metadata).is_err());
        let too_many_resources = SkillsConfig {
            max_resources: 2_049,
            ..SkillsConfig::default()
        };
        assert!(validate_skills(&too_many_resources).is_err());
        let oversized_resource = SkillsConfig {
            max_resource_bytes: 4 * 1024 * 1024 + 1,
            ..SkillsConfig::default()
        };
        assert!(validate_skills(&oversized_resource).is_err());
    }

    #[test]
    fn mcp_config_is_typed_bounded_and_fail_closed() -> Result<(), Box<dyn std::error::Error>> {
        let file: FileConfig = toml::from_str(
            r#"
                [mcp]
                enabled = true
                reconnect_max_attempts = 2

                [[mcp.servers]]
                name = "files"
                transport = "stdio"
                command = "fixture-server"
                args = ["--stdio"]
                approval = "writes"
                trusted_read_only_tools = ["read"]
                disabled_tools = ["delete"]
            "#,
        )?;
        let config = build_mcp_config(file.mcp.unwrap_or_default())?;
        assert!(config.enabled);
        assert_eq!(config.reconnect_max_attempts, 2);
        assert_eq!(config.servers.len(), 1);
        assert!(matches!(
            config.servers[0].permissions.approval,
            crate::mcp::McpApprovalMode::Writes
        ));
        assert!(
            config.servers[0]
                .permissions
                .disabled_tools
                .contains("delete")
        );

        let invalid: FileConfig = toml::from_str(
            r#"
                [mcp]
                [[mcp.servers]]
                name = "remote"
                transport = "streamable_http"
                url = "http://remote.example.test/mcp"
            "#,
        )?;
        assert!(build_mcp_config(invalid.mcp.unwrap_or_default()).is_err());
        Ok(())
    }

    #[test]
    fn enabled_plugin_connections_add_only_server_entries() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let namespace = root.path().join("dev-example");
        std::fs::create_dir_all(namespace.join("mcp"))?;
        std::fs::create_dir_all(namespace.join("lsp"))?;
        std::fs::write(
            namespace.join("mcp/server.toml"),
            r#"
                [[servers]]
                name = "plugin-docs"
                transport = "http"
                url = "https://example.test/mcp"
                approval = "always"
            "#,
        )?;
        std::fs::write(
            namespace.join("lsp/server.toml"),
            r#"
                [[servers]]
                name = "plugin-rust"
                command = "rust-analyzer"
                language_id = "rust"
                extensions = ["rs"]
            "#,
        )?;
        let mut mcp = FileMcpConfig::default();
        let mut lsp = FileLspConfig::default();
        merge_plugin_connections(root.path(), &mut mcp, &mut lsp)?;
        assert_eq!(mcp.servers.len(), 1);
        assert_eq!(lsp.servers.len(), 1);

        std::fs::write(
            namespace.join("mcp/unsafe.toml"),
            "enabled = true\n[[servers]]\nname='bad'\ntransport='http'\nurl='https://example.test'\n",
        )?;
        assert!(
            merge_plugin_connections(
                root.path(),
                &mut FileMcpConfig::default(),
                &mut FileLspConfig::default(),
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn lsp_config_is_trusted_typed_and_never_implies_installation()
    -> Result<(), Box<dyn std::error::Error>> {
        let file: FileConfig = toml::from_str(
            r#"
                [lsp]
                enabled = true
                request_timeout_secs = 7

                [[lsp.servers]]
                name = "rust-analyzer"
                command = "rust-analyzer"
                args = []
                language_id = "rust"
                extensions = [".rs"]
                root_markers = ["Cargo.toml"]
                auto_start = false
            "#,
        )?;
        let config = build_lsp_config(file.lsp.unwrap_or_default())?;
        assert!(config.enabled);
        assert_eq!(config.request_timeout, Duration::from_secs(7));
        assert_eq!(config.servers.len(), 1);
        assert!(!config.servers[0].auto_start);
        assert_eq!(config.servers[0].command, "rust-analyzer");

        let invalid: FileConfig = toml::from_str(
            r#"
                [lsp]
                [[lsp.servers]]
                name = "escape"
                command = "server"
                language_id = "x"
                root_markers = ["../outside"]
            "#,
        )?;
        assert!(build_lsp_config(invalid.lsp.unwrap_or_default()).is_err());
        Ok(())
    }

    #[test]
    fn code_index_config_is_bounded_local_and_incremental() -> Result<(), Box<dyn std::error::Error>>
    {
        let file: FileConfig = toml::from_str(
            r#"
                [code_index]
                enabled = true
                auto_refresh = false
                max_files = 1200
                max_file_bytes = 65536
                max_source_bytes = 1048576
                max_chunks = 4000
                chunk_lines = 80
                overlap_lines = 8
                max_result_bytes = 32768
            "#,
        )?;
        let config =
            build_code_index_config(file.code_index.unwrap_or_default(), &valid_api_config())?;
        assert!(config.enabled);
        assert!(!config.auto_refresh);
        assert_eq!(config.chunk_lines, 80);
        assert_eq!(config.overlap_lines, 8);

        let invalid: FileConfig = toml::from_str(
            r#"
                [code_index]
                chunk_lines = 20
                overlap_lines = 20
            "#,
        )?;
        assert!(
            build_code_index_config(invalid.code_index.unwrap_or_default(), &valid_api_config())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn vector_search_is_explicit_provider_bound_and_secret_redacted()
    -> Result<(), Box<dyn std::error::Error>> {
        let file: FileConfig = toml::from_str(
            r#"
                [code_index]
                enabled = true

                [code_index.embeddings]
                enabled = true
                model = "embedding-deployment"
                dimensions = 1536
                batch_size = 16
                hybrid_weight = 0.7
            "#,
        )?;
        let config =
            build_code_index_config(file.code_index.unwrap_or_default(), &valid_api_config())?;
        assert!(config.embeddings.enabled);
        assert_eq!(
            config.embeddings.endpoint,
            "https://example.test/openai/v1/embeddings"
        );
        assert_eq!(config.embeddings.provider, ApiProvider::Azure);
        assert_eq!(config.embeddings.auth, ApiAuth::ApiKey);
        let debug = format!("{:?}", config.embeddings);
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("secret"));

        let invalid: FileConfig = toml::from_str(
            r#"
                [code_index.embeddings]
                enabled = true
                model = "embedding-deployment"
                hybrid_weight = 1.5
            "#,
        )?;
        assert!(
            build_code_index_config(invalid.code_index.unwrap_or_default(), &valid_api_config())
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn explicitly_selected_missing_config_is_fatal() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let missing = directory.path().join("missing.toml");
        assert!(matches!(
            AppConfig::load_file_config(Some(&missing), None),
            Err(ConfigError::ConfigIo { path, .. }) if path == missing
        ));
        Ok(())
    }

    #[test]
    fn project_config_is_size_bounded() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        std::fs::write(
            directory.path().join(".decode.toml"),
            vec![b' '; 256 * 1024 + 1],
        )?;
        assert!(matches!(
            AppConfig::load_file_config(None, Some(directory.path())),
            Err(ConfigError::InvalidValue {
                field: "project config",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn server_compaction_threshold_has_the_exact_wire_shape() {
        let mut config = valid_api_config();
        config.server_compaction_threshold = Some(32_000);
        assert_eq!(
            config.context_management(),
            Some(vec![serde_json::json!({
                "type": "compaction",
                "compact_threshold": 32_000,
            })])
        );
        config.server_compaction_threshold = Some(0);
        assert!(config.validate().is_err());
    }

    #[test]
    fn deployment_pricing_is_explicit_unique_and_finite() {
        let entry = FileDeploymentPricing {
            deployment: "azure-prod".to_owned(),
            input_usd_per_million: 1.25,
            cached_input_usd_per_million: Some(0.125),
            output_usd_per_million: 10.0,
            long_context_threshold_tokens: Some(200_000),
            long_context_input_usd_per_million: Some(2.5),
            long_context_cached_input_usd_per_million: Some(0.25),
            long_context_output_usd_per_million: Some(15.0),
        };
        assert!(build_pricing_catalog(ApiProvider::Azure, std::slice::from_ref(&entry)).is_ok());
        assert!(
            build_pricing_catalog(
                ApiProvider::Azure,
                &[
                    entry,
                    FileDeploymentPricing {
                        deployment: "azure-prod".to_owned(),
                        input_usd_per_million: 2.0,
                        cached_input_usd_per_million: None,
                        output_usd_per_million: 8.0,
                        long_context_threshold_tokens: None,
                        long_context_input_usd_per_million: None,
                        long_context_cached_input_usd_per_million: None,
                        long_context_output_usd_per_million: None,
                    }
                ]
            )
            .is_err()
        );
        assert!(
            build_pricing_catalog(
                ApiProvider::Azure,
                &[FileDeploymentPricing {
                    deployment: "bad".to_owned(),
                    input_usd_per_million: f64::INFINITY,
                    cached_input_usd_per_million: None,
                    output_usd_per_million: 1.0,
                    long_context_threshold_tokens: None,
                    long_context_input_usd_per_million: None,
                    long_context_cached_input_usd_per_million: None,
                    long_context_output_usd_per_million: None,
                }]
            )
            .is_err()
        );
    }

    #[test]
    fn verified_google_pricing_uses_the_per_response_long_context_tier()
    -> Result<(), Box<dyn std::error::Error>> {
        let catalog = build_pricing_catalog(ApiProvider::Google, &[])?;
        let mut ledger = UsageLedger::default();
        ledger.record("gemini-3.1-pro-preview", 250_000, 50_000, 10_000, 260_000);

        let snapshot = catalog.snapshot(&ledger, Some(260_000));

        // 200k uncached * $4/M + 50k cached * $0.4/M + 10k output * $18/M.
        assert_eq!(snapshot.estimated_cost_microusd, 1_000_000);
        assert!(!snapshot.has_unpriced_usage);
        Ok(())
    }

    #[test]
    fn official_openai_standard_pricing_is_available_for_supported_models()
    -> Result<(), Box<dyn std::error::Error>> {
        let catalog = build_pricing_catalog(ApiProvider::OpenAi, &[])?;
        let mut ledger = UsageLedger::default();
        ledger.record("gpt-5.6-sol", 1_000_000, 100_000, 1_000_000, 2_000_000);
        let snapshot = catalog.snapshot(&ledger, Some(2_000_000));
        let rate = snapshot.deployments[0]
            .pricing
            .ok_or("missing official gpt-5.6-sol rate")?;
        assert_eq!(rate.input_usd_per_million(), 4.0);
        assert_eq!(rate.cached_input_usd_per_million(), 0.4);
        assert_eq!(rate.output_usd_per_million(), 20.0);
        assert!(!snapshot.has_unpriced_usage);
        Ok(())
    }

    #[test]
    fn interactive_terminal_config_is_bounded_and_rejects_nul() {
        let config = build_interactive_terminal_config(FileInteractiveTerminalConfig {
            enabled: Some(true),
            program: Some("  pwsh.exe  ".to_owned()),
            args: vec!["-NoLogo".to_owned()],
            max_sessions: Some(8),
            scrollback_lines: Some(20_000),
        });
        assert!(matches!(
            config,
            Ok(config)
                if config.program.as_deref() == Some("pwsh.exe")
                    && config.max_sessions == 8
                    && config.scrollback_lines == 20_000
        ));
        assert!(
            build_interactive_terminal_config(FileInteractiveTerminalConfig {
                max_sessions: Some(0),
                ..FileInteractiveTerminalConfig::default()
            })
            .is_err()
        );
        assert!(
            build_interactive_terminal_config(FileInteractiveTerminalConfig {
                program: Some("bad\0shell".to_owned()),
                ..FileInteractiveTerminalConfig::default()
            })
            .is_err()
        );
        assert!(
            build_interactive_terminal_config(FileInteractiveTerminalConfig {
                scrollback_lines: Some(99),
                ..FileInteractiveTerminalConfig::default()
            })
            .is_err()
        );
    }
}
