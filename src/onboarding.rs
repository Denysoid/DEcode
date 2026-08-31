use std::{
    io::IsTerminal,
    path::{Path, PathBuf},
};

use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    config::{ResponsesEndpoint, UiLanguage, build_project_root},
    error::ConfigError,
};

pub const CONTEXT_CHOICES: [u32; 6] = [100_000, 200_000, 400_000, 800_000, 1_000_000, 2_000_000];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SetupProvider {
    #[default]
    Azure,
    OpenAi,
    Google,
    Anthropic,
    /// OpenAI-compatible Bedrock Mantle endpoint.
    AwsBedrock,
    /// Native Bedrock Runtime using the AWS SDK credential chain and SigV4.
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

impl SetupProvider {
    pub const ALL: [Self; 23] = [
        Self::Azure,
        Self::OpenAi,
        Self::Google,
        Self::Anthropic,
        Self::AwsBedrock,
        Self::AwsBedrockRuntime,
        Self::OpenRouter,
        Self::XAi,
        Self::Groq,
        Self::Mistral,
        Self::DeepSeek,
        Self::Together,
        Self::Fireworks,
        Self::Cerebras,
        Self::Perplexity,
        Self::Nvidia,
        Self::SambaNova,
        Self::Moonshot,
        Self::Alibaba,
        Self::HuggingFace,
        Self::GitHubModels,
        Self::Ollama,
        Self::Compatible,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Azure => "azure",
            Self::OpenAi => "openai",
            Self::Google => "google",
            Self::Anthropic => "anthropic",
            Self::AwsBedrock => "bedrock_mantle",
            Self::AwsBedrockRuntime => "bedrock_runtime",
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
            Self::GitHubModels => "github_models",
            Self::Ollama => "ollama",
            Self::Compatible => "compatible",
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
            Self::AwsBedrockRuntime => "AWS Bedrock Runtime (native SigV4)",
            Self::OpenRouter => "OpenRouter",
            Self::XAi => "xAI / Grok",
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
            Self::Ollama => "Ollama (local)",
            Self::Compatible => "Custom compatible",
        }
    }

    #[must_use]
    pub const fn needs_endpoint(self) -> bool {
        matches!(self, Self::Azure | Self::AwsBedrock | Self::Compatible)
    }

    #[must_use]
    pub const fn needs_secret(self) -> bool {
        !matches!(self, Self::Ollama | Self::AwsBedrockRuntime)
    }

    #[must_use]
    pub const fn auth(self) -> &'static str {
        match self {
            Self::Azure => "api_key",
            Self::Anthropic => "anthropic_key",
            Self::Google => "google_key",
            Self::AwsBedrockRuntime => "aws_sdk",
            Self::OpenAi
            | Self::AwsBedrock
            | Self::OpenRouter
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
            | Self::Ollama
            | Self::Compatible => "bearer",
        }
    }

    #[must_use]
    pub const fn suggested_model(self) -> &'static str {
        match self {
            Self::Azure => "your-deployment",
            Self::OpenAi => "gpt-5.2-codex",
            Self::Google => "gemini-2.5-pro",
            Self::Anthropic => "claude-sonnet-4-5",
            Self::AwsBedrock => "your-inference-profile",
            Self::AwsBedrockRuntime => "your-bedrock-model-or-inference-profile",
            Self::OpenRouter => "openai/gpt-5.2-codex",
            Self::XAi => "grok-code-fast-1",
            Self::Groq => "openai/gpt-oss-120b",
            Self::Mistral => "codestral-latest",
            Self::DeepSeek => "deepseek-chat",
            Self::Together => "openai/gpt-oss-120b",
            Self::Fireworks => "accounts/fireworks/models/kimi-k2p5",
            Self::Cerebras => "gpt-oss-120b",
            Self::Perplexity => "sonar-pro",
            Self::Nvidia => "meta/llama-3.3-70b-instruct",
            Self::SambaNova => "Meta-Llama-3.3-70B-Instruct",
            Self::Moonshot => "kimi-k2.5",
            Self::Alibaba => "qwen3-coder-plus",
            Self::HuggingFace => "Qwen/Qwen3-Coder-Next",
            Self::GitHubModels => "openai/gpt-4.1",
            Self::Ollama => "qwen3-coder",
            Self::Compatible => "your-model",
        }
    }
}

#[derive(Clone)]
pub struct OnboardingAnswers {
    pub language: UiLanguage,
    pub provider: SetupProvider,
    pub model: String,
    pub endpoint: String,
    pub api_key: SecretString,
    pub workspace: PathBuf,
    pub context_budget: u32,
    pub use_case: String,
    pub api_transport: String,
    pub request_timeout_secs: u64,
    pub stream_idle_timeout_secs: u64,
    pub max_attempts: u32,
    pub retry_min_delay_ms: u64,
    pub retry_max_delay_secs: u64,
    pub retry_after_cap_secs: u64,
    pub aws_region: Option<String>,
    pub aws_profile: Option<String>,
    pub aws_role_arn: Option<String>,
    pub bedrock_endpoint_url: Option<String>,
}

impl std::fmt::Debug for OnboardingAnswers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OnboardingAnswers")
            .field("language", &self.language)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("endpoint", &self.endpoint)
            .field("api_key", &"[REDACTED]")
            .field("workspace", &self.workspace)
            .field("context_budget", &self.context_budget)
            .field("use_case", &self.use_case)
            .field("api_transport", &self.api_transport)
            .field("request_timeout_secs", &self.request_timeout_secs)
            .field("stream_idle_timeout_secs", &self.stream_idle_timeout_secs)
            .field("max_attempts", &self.max_attempts)
            .field("retry_min_delay_ms", &self.retry_min_delay_ms)
            .field("retry_max_delay_secs", &self.retry_max_delay_secs)
            .field("retry_after_cap_secs", &self.retry_after_cap_secs)
            .field("aws_region", &self.aws_region)
            .field("aws_profile", &self.aws_profile)
            .field("aws_role_arn", &self.aws_role_arn)
            .field("bedrock_endpoint_url", &self.bedrock_endpoint_url)
            .finish()
    }
}

impl Default for OnboardingAnswers {
    fn default() -> Self {
        Self {
            language: UiLanguage::English,
            provider: SetupProvider::Azure,
            model: SetupProvider::Azure.suggested_model().to_owned(),
            endpoint: String::new(),
            api_key: SecretString::new(String::new().into()),
            workspace: default_workspace(),
            context_budget: 200_000,
            use_case: "Coding, review, tests, and project maintenance".to_owned(),
            api_transport: "auto".to_owned(),
            request_timeout_secs: 120,
            stream_idle_timeout_secs: 180,
            max_attempts: 5,
            retry_min_delay_ms: 500,
            retry_max_delay_secs: 30,
            retry_after_cap_secs: 120,
            aws_region: None,
            aws_profile: None,
            aws_role_arn: None,
            bedrock_endpoint_url: None,
        }
    }
}

fn default_workspace() -> PathBuf {
    let current = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let from_current = default_workspace_from(&current);
    if from_current != current {
        return from_current;
    }
    std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().and_then(build_project_root))
        .unwrap_or(current)
}

fn default_workspace_from(path: &Path) -> PathBuf {
    build_project_root(path).unwrap_or_else(|| path.to_path_buf())
}

#[derive(Debug, Error)]
pub enum OnboardingError {
    #[error("cannot determine the DEcode configuration directory")]
    NoConfigDirectory,
    #[error("onboarding field {field} is invalid: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("onboarding I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("OS credential store failed: {0}")]
    CredentialStore(String),
    #[error("onboarding config serialization failed: {0}")]
    Serialize(String),
    #[error("onboarding preferences are invalid at {path}: {message}")]
    Preferences { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UiPreferences {
    pub language: UiLanguage,
    pub onboarding_completed: bool,
    pub mascot_enabled: Option<bool>,
    pub default_context_budget: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredUiPreferences {
    version: u32,
    language: String,
    onboarding_completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mascot_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_context_budget: Option<u32>,
}

const PREFERENCES_FILE: &str = "ui-preferences.toml";
const MAX_PREFERENCES_BYTES: u64 = 16 * 1024;

pub fn load_ui_preferences() -> Result<Option<UiPreferences>, OnboardingError> {
    let directories = directories::ProjectDirs::from("dev", "denysoid", "decode")
        .ok_or(OnboardingError::NoConfigDirectory)?;
    load_ui_preferences_from(directories.config_dir())
}

pub fn persist_ui_preferences(
    language: UiLanguage,
    onboarding_completed: bool,
) -> Result<PathBuf, OnboardingError> {
    let directories = directories::ProjectDirs::from("dev", "denysoid", "decode")
        .ok_or(OnboardingError::NoConfigDirectory)?;
    persist_ui_preferences_to(directories.config_dir(), language, onboarding_completed)
}

pub fn persist_mascot_preference(
    language: UiLanguage,
    onboarding_completed: bool,
    mascot_enabled: bool,
) -> Result<PathBuf, OnboardingError> {
    let directories = directories::ProjectDirs::from("dev", "denysoid", "decode")
        .ok_or(OnboardingError::NoConfigDirectory)?;
    persist_mascot_preference_to(
        directories.config_dir(),
        language,
        onboarding_completed,
        mascot_enabled,
    )
}

pub fn persist_default_context_budget(
    language: UiLanguage,
    onboarding_completed: bool,
    context_budget: u32,
) -> Result<PathBuf, OnboardingError> {
    if context_budget == 0 || context_budget > crate::config::MAX_CONTEXT_BUDGET {
        return Err(OnboardingError::Invalid {
            field: "default_context_budget",
            message: format!(
                "must be between 1 and {}",
                crate::config::MAX_CONTEXT_BUDGET
            ),
        });
    }
    let directories = directories::ProjectDirs::from("dev", "denysoid", "decode")
        .ok_or(OnboardingError::NoConfigDirectory)?;
    persist_default_context_budget_to(
        directories.config_dir(),
        language,
        onboarding_completed,
        context_budget,
    )
}

fn load_ui_preferences_from(config_dir: &Path) -> Result<Option<UiPreferences>, OnboardingError> {
    let path = config_dir.join(PREFERENCES_FILE);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(OnboardingError::Io { path, source }),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(OnboardingError::Preferences {
            path,
            message: "preferences must be a regular, non-symlink file".to_owned(),
        });
    }
    if metadata.len() > MAX_PREFERENCES_BYTES {
        return Err(OnboardingError::Preferences {
            path,
            message: format!("file exceeds {MAX_PREFERENCES_BYTES} bytes"),
        });
    }
    let encoded = std::fs::read_to_string(&path).map_err(|source| OnboardingError::Io {
        path: path.clone(),
        source,
    })?;
    let stored = toml::from_str::<StoredUiPreferences>(&encoded).map_err(|error| {
        OnboardingError::Preferences {
            path: path.clone(),
            message: error.to_string(),
        }
    })?;
    if stored.version != 1 {
        return Err(OnboardingError::Preferences {
            path,
            message: format!("unsupported preferences version {}", stored.version),
        });
    }
    let language = UiLanguage::parse(Some(&stored.language)).map_err(|error| {
        OnboardingError::Preferences {
            path: path.clone(),
            message: error.to_string(),
        }
    })?;
    if stored
        .default_context_budget
        .is_some_and(|budget| budget == 0 || budget > crate::config::MAX_CONTEXT_BUDGET)
    {
        return Err(OnboardingError::Preferences {
            path,
            message: format!(
                "default_context_budget must be between 1 and {}",
                crate::config::MAX_CONTEXT_BUDGET
            ),
        });
    }
    Ok(Some(UiPreferences {
        language,
        onboarding_completed: stored.onboarding_completed,
        mascot_enabled: stored.mascot_enabled,
        default_context_budget: stored.default_context_budget,
    }))
}

fn persist_ui_preferences_to(
    config_dir: &Path,
    language: UiLanguage,
    onboarding_completed: bool,
) -> Result<PathBuf, OnboardingError> {
    update_ui_preferences_to(config_dir, language, onboarding_completed, |preferences| {
        preferences.language = language;
        preferences.onboarding_completed = onboarding_completed;
    })
}

fn persist_mascot_preference_to(
    config_dir: &Path,
    language: UiLanguage,
    onboarding_completed: bool,
    mascot_enabled: bool,
) -> Result<PathBuf, OnboardingError> {
    update_ui_preferences_to(config_dir, language, onboarding_completed, |preferences| {
        preferences.mascot_enabled = Some(mascot_enabled);
    })
}

fn persist_default_context_budget_to(
    config_dir: &Path,
    language: UiLanguage,
    onboarding_completed: bool,
    context_budget: u32,
) -> Result<PathBuf, OnboardingError> {
    update_ui_preferences_to(config_dir, language, onboarding_completed, |preferences| {
        preferences.default_context_budget = Some(context_budget);
    })
}

fn update_ui_preferences_to(
    config_dir: &Path,
    language: UiLanguage,
    onboarding_completed: bool,
    update: impl FnOnce(&mut UiPreferences),
) -> Result<PathBuf, OnboardingError> {
    std::fs::create_dir_all(config_dir).map_err(|source| OnboardingError::Io {
        path: config_dir.to_path_buf(),
        source,
    })?;
    let mut preferences = load_ui_preferences_from(config_dir)?.unwrap_or(UiPreferences {
        language,
        onboarding_completed,
        mascot_enabled: None,
        default_context_budget: None,
    });
    update(&mut preferences);
    let path = config_dir.join(PREFERENCES_FILE);
    let encoded = toml::to_string_pretty(&StoredUiPreferences {
        version: 1,
        language: preferences.language.code().to_owned(),
        onboarding_completed: preferences.onboarding_completed,
        mascot_enabled: preferences.mascot_enabled,
        default_context_budget: preferences.default_context_budget,
    })
    .map_err(|error| OnboardingError::Serialize(error.to_string()))?;
    atomic_write(&path, encoded.as_bytes())?;
    Ok(path)
}

#[must_use]
pub fn should_offer(error: &ConfigError) -> bool {
    should_offer_with_context(
        error,
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        std::env::args_os().skip(1),
    )
}

fn should_offer_with_context(
    error: &ConfigError,
    interactive: bool,
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> bool {
    if !interactive {
        return false;
    }
    if explicit_config_requested(arguments) {
        return false;
    }
    matches!(
        error,
        ConfigError::MissingApiKey
            | ConfigError::MissingResponsesUrl
            | ConfigError::MissingDeployment
            | ConfigError::MissingInstructionsFile
            | ConfigError::InvalidValue {
                field: "api.responses_url" | "api.azure_base_url",
                ..
            }
    )
}

fn explicit_config_requested(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> bool {
    for argument in arguments {
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        if argument == "--config"
            || argument.starts_with("--config=")
            || argument == "--config-file"
            || argument.starts_with("--config-file=")
            || argument == "-c"
            || (argument.starts_with("-c") && argument.len() > 2)
        {
            return true;
        }
    }
    false
}

#[must_use]
pub fn should_launch(completed: bool) -> bool {
    !completed && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

pub fn persist(answers: &OnboardingAnswers) -> Result<PathBuf, OnboardingError> {
    let directories = directories::ProjectDirs::from("dev", "denysoid", "decode")
        .ok_or(OnboardingError::NoConfigDirectory)?;
    persist_to(
        answers,
        directories.config_dir(),
        Some(directories.data_local_dir()),
        true,
    )
}

pub fn persist_local_fallback(language: UiLanguage) -> Result<PathBuf, OnboardingError> {
    persist(&local_fallback_answers(language))
}

fn local_fallback_answers(language: UiLanguage) -> OnboardingAnswers {
    OnboardingAnswers {
        language,
        provider: SetupProvider::Ollama,
        model: SetupProvider::Ollama.suggested_model().to_owned(),
        ..OnboardingAnswers::default()
    }
}

fn persist_to(
    answers: &OnboardingAnswers,
    config_dir: &Path,
    data_dir: Option<&Path>,
    store_secret: bool,
) -> Result<PathBuf, OnboardingError> {
    validate_answers(answers)?;
    std::fs::create_dir_all(config_dir).map_err(|source| OnboardingError::Io {
        path: config_dir.to_path_buf(),
        source,
    })?;
    let instructions_path = config_dir.join("instructions.md");
    let account = format!("{}-primary", answers.provider.id());
    let workspace = answers.workspace.to_string_lossy();
    let instructions = instructions_path.to_string_lossy();
    let endpoint = answers.endpoint.trim();
    let azure_full_url = answers.provider == SetupProvider::Azure && ends_in_responses(endpoint);
    let config = StoredConfig {
        api: StoredApi {
            provider: answers.provider.id(),
            provider_auth: answers.provider.auth(),
            keyring_account: answers.provider.needs_secret().then_some(account.as_str()),
            responses_url: matches!(
                answers.provider,
                SetupProvider::AwsBedrock | SetupProvider::Compatible
            )
            .then_some(endpoint)
            .or(azure_full_url.then_some(endpoint)),
            azure_base_url: (answers.provider == SetupProvider::Azure && !azure_full_url)
                .then_some(endpoint),
            deployment: answers.model.trim(),
            deployment_choices: vec![answers.model.trim()],
            max_output_tokens: 8_192,
            reasoning_effort: "high",
            transport: answers.api_transport.as_str(),
            request_timeout_secs: answers.request_timeout_secs,
            stream_idle_timeout_secs: answers.stream_idle_timeout_secs,
            max_attempts: answers.max_attempts,
            retry_min_delay_ms: answers.retry_min_delay_ms,
            retry_max_delay_secs: answers.retry_max_delay_secs,
            retry_after_cap_secs: answers.retry_after_cap_secs,
            aws_region: answers.aws_region.as_deref(),
            aws_profile: answers.aws_profile.as_deref(),
            aws_role_arn: answers.aws_role_arn.as_deref(),
            bedrock_endpoint_url: answers.bedrock_endpoint_url.as_deref(),
        },
        agent: StoredAgent {
            context_mode: "stateless",
            context_budget: answers.context_budget,
            max_context_budget: crate::config::MAX_CONTEXT_BUDGET,
            workspace_root: &workspace,
            session_dir: data_dir.map(|dir| dir.join("sessions").to_string_lossy().into_owned()),
            instructions_file: &instructions,
        },
        ui: StoredUi {
            confirm_destructive: true,
            mouse_enabled: true,
            language: answers.language.code(),
            onboarding_completed: true,
            mascot_enabled: true,
        },
    };
    let config_path = config_dir.join("config.toml");
    let encoded = merge_setup_config(&config_path, &config)?;
    ensure_default_instructions(&instructions_path, answers)?;
    if store_secret && answers.provider.needs_secret() {
        let entry = keyring::Entry::new("decode-provider", &account)
            .map_err(|error| OnboardingError::CredentialStore(error.to_string()))?;
        entry
            .set_password(answers.api_key.expose_secret())
            .map_err(|error| OnboardingError::CredentialStore(error.to_string()))?;
    }
    atomic_write(&config_path, encoded.as_bytes())?;
    update_ui_preferences_to(config_dir, answers.language, true, |preferences| {
        preferences.language = answers.language;
        preferences.onboarding_completed = true;
        preferences.default_context_budget = Some(answers.context_budget);
    })?;
    Ok(config_path)
}

fn ensure_default_instructions(
    path: &Path,
    answers: &OnboardingAnswers,
) -> Result<(), OnboardingError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(OnboardingError::Invalid {
                field: "instructions_file",
                message: "existing instructions path must be a regular non-symlink file".to_owned(),
            });
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(OnboardingError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    atomic_write(
        path,
        format!(
            "# DEcode agent instructions\n\nPrimary UI language: {}.\nPrimary purpose: {}.\nPlan before risky changes, preserve user work, verify every change, and report failures honestly.\n",
            answers.language.code(),
            answers.use_case.trim()
        )
        .as_bytes(),
    )
}

fn merge_setup_config(path: &Path, setup: &StoredConfig<'_>) -> Result<String, OnboardingError> {
    const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
    let mut root = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(OnboardingError::Invalid {
                    field: "config",
                    message: "existing config must be a regular non-symlink file".to_owned(),
                });
            }
            if metadata.len() > MAX_CONFIG_BYTES {
                return Err(OnboardingError::Invalid {
                    field: "config",
                    message: format!("existing config exceeds {MAX_CONFIG_BYTES} bytes"),
                });
            }
            let encoded = std::fs::read_to_string(path).map_err(|source| OnboardingError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            toml::from_str::<toml::Value>(&encoded)
                .map_err(|error| OnboardingError::Serialize(error.to_string()))?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            toml::Value::Table(toml::map::Map::new())
        }
        Err(source) => {
            return Err(OnboardingError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let desired = toml::Value::try_from(setup)
        .map_err(|error| OnboardingError::Serialize(error.to_string()))?;
    let root_table = root
        .as_table_mut()
        .ok_or_else(|| OnboardingError::Serialize("config root must be a TOML table".to_owned()))?;
    let desired_table = desired
        .as_table()
        .ok_or_else(|| OnboardingError::Serialize("generated config is not a table".to_owned()))?;
    overlay_setup_section(
        root_table,
        desired_table,
        "api",
        &[
            "provider",
            "provider_auth",
            "keyring_account",
            "responses_url",
            "azure_base_url",
            "deployment",
            "model",
            "deployment_choices",
            "api_version",
            "max_output_tokens",
            "reasoning_effort",
            "transport",
            "request_timeout_secs",
            "stream_idle_timeout_secs",
            "max_attempts",
            "retry_min_delay_ms",
            "retry_max_delay_secs",
            "retry_after_cap_secs",
            "aws_region",
            "aws_profile",
            "aws_role_arn",
            "bedrock_endpoint_url",
        ],
    )?;
    overlay_setup_section(
        root_table,
        desired_table,
        "agent",
        &[
            "context_mode",
            "context_budget",
            "max_context_budget",
            "workspace_root",
            "session_dir",
            "instructions_file",
        ],
    )?;
    overlay_setup_section(
        root_table,
        desired_table,
        "ui",
        &[
            "confirm_destructive",
            "mouse_enabled",
            "language",
            "onboarding_completed",
            "mascot_enabled",
        ],
    )?;
    toml::to_string_pretty(&root).map_err(|error| OnboardingError::Serialize(error.to_string()))
}

fn overlay_setup_section(
    root: &mut toml::map::Map<String, toml::Value>,
    desired: &toml::map::Map<String, toml::Value>,
    section: &'static str,
    owned_keys: &[&str],
) -> Result<(), OnboardingError> {
    let destination = root
        .entry(section.to_owned())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut()
        .ok_or_else(|| OnboardingError::Invalid {
            field: section,
            message: "existing section must be a TOML table".to_owned(),
        })?;
    for key in owned_keys {
        destination.remove(*key);
    }
    let source = desired
        .get(section)
        .and_then(toml::Value::as_table)
        .ok_or_else(|| OnboardingError::Serialize(format!("missing generated [{section}]")))?;
    for (key, value) in source {
        destination.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn validate_answers(answers: &OnboardingAnswers) -> Result<(), OnboardingError> {
    let visible = |value: &str, max: usize| {
        !value.trim().is_empty() && value.len() <= max && !value.chars().any(char::is_control)
    };
    if !visible(&answers.model, 256) {
        return Err(OnboardingError::Invalid {
            field: "model",
            message: "enter a visible model/deployment name of at most 256 bytes".to_owned(),
        });
    }
    if answers.use_case.len() > 4_096 || answers.use_case.chars().any(char::is_control) {
        return Err(OnboardingError::Invalid {
            field: "use_case",
            message: "use case must be control-free and at most 4096 bytes".to_owned(),
        });
    }
    if answers.provider.needs_endpoint() && !visible(&answers.endpoint, 2_048) {
        return Err(OnboardingError::Invalid {
            field: "endpoint",
            message: "this provider requires an HTTPS endpoint".to_owned(),
        });
    }
    if answers.provider.needs_endpoint() {
        validate_provider_endpoint(answers.provider, "endpoint", answers.endpoint.trim())?;
    }
    if answers.provider.needs_secret() {
        let secret = answers.api_key.expose_secret();
        if secret.trim().is_empty() || secret.len() > 4_096 || secret.chars().any(char::is_control)
        {
            return Err(OnboardingError::Invalid {
                field: "api_key",
                message: "enter a visible provider API key of at most 4096 bytes".to_owned(),
            });
        }
    }
    if !answers.workspace.is_absolute() || !answers.workspace.is_dir() {
        return Err(OnboardingError::Invalid {
            field: "workspace",
            message: "choose an existing absolute directory".to_owned(),
        });
    }
    if !CONTEXT_CHOICES.contains(&answers.context_budget) {
        return Err(OnboardingError::Invalid {
            field: "context_budget",
            message: "choose one of the bounded context presets".to_owned(),
        });
    }
    if !matches!(answers.api_transport.as_str(), "auto" | "sse" | "websocket") {
        return Err(OnboardingError::Invalid {
            field: "api.transport",
            message: "choose auto, sse, or websocket".to_owned(),
        });
    }
    if answers.api_transport == "websocket" && answers.provider != SetupProvider::OpenAi {
        return Err(OnboardingError::Invalid {
            field: "api.transport",
            message: "websocket is available only for the official OpenAI provider".to_owned(),
        });
    }
    if answers.request_timeout_secs == 0 || answers.request_timeout_secs > 86_400 {
        return Err(OnboardingError::Invalid {
            field: "api.request_timeout_secs",
            message: "must be in 1..=86400".to_owned(),
        });
    }
    if answers.stream_idle_timeout_secs == 0 || answers.stream_idle_timeout_secs > 86_400 {
        return Err(OnboardingError::Invalid {
            field: "api.stream_idle_timeout_secs",
            message: "must be in 1..=86400".to_owned(),
        });
    }
    if !(1..=5).contains(&answers.max_attempts) {
        return Err(OnboardingError::Invalid {
            field: "api.max_attempts",
            message: "must be in 1..=5".to_owned(),
        });
    }
    if answers.retry_min_delay_ms == 0
        || answers.retry_max_delay_secs == 0
        || answers.retry_max_delay_secs > 30
        || answers.retry_min_delay_ms > answers.retry_max_delay_secs.saturating_mul(1_000)
    {
        return Err(OnboardingError::Invalid {
            field: "api.retry",
            message: "retry delay must be positive and min must not exceed max (30s)".to_owned(),
        });
    }
    if answers.retry_after_cap_secs == 0 || answers.retry_after_cap_secs > 120 {
        return Err(OnboardingError::Invalid {
            field: "api.retry_after_cap_secs",
            message: "must be in 1..=120".to_owned(),
        });
    }
    for (field, value, limit) in [
        ("api.aws_region", answers.aws_region.as_deref(), 64),
        ("api.aws_profile", answers.aws_profile.as_deref(), 128),
        ("api.aws_role_arn", answers.aws_role_arn.as_deref(), 2_048),
        (
            "api.bedrock_endpoint_url",
            answers.bedrock_endpoint_url.as_deref(),
            2_048,
        ),
    ] {
        if let Some(value) = value
            && (value.trim().is_empty()
                || value.len() > limit
                || value.chars().any(char::is_control))
        {
            return Err(OnboardingError::Invalid {
                field,
                message: "must be a bounded visible value".to_owned(),
            });
        }
    }
    if let Some(endpoint) = answers.bedrock_endpoint_url.as_deref() {
        validate_https_endpoint("api.bedrock_endpoint_url", endpoint.trim())?;
    }
    Ok(())
}

pub(crate) fn validate_https_endpoint(
    field: &'static str,
    endpoint: &str,
) -> Result<(), OnboardingError> {
    ResponsesEndpoint::FullUrl(endpoint.to_owned())
        .resolved_url(false)
        .map(|_| ())
        .map_err(|error| OnboardingError::Invalid {
            field,
            message: error.to_string(),
        })
}

pub(crate) fn validate_provider_endpoint(
    provider: SetupProvider,
    field: &'static str,
    endpoint: &str,
) -> Result<(), OnboardingError> {
    let endpoint = if provider == SetupProvider::Azure && !ends_in_responses(endpoint) {
        ResponsesEndpoint::AzureBaseUrl(endpoint.to_owned())
    } else {
        ResponsesEndpoint::FullUrl(endpoint.to_owned())
    };
    endpoint
        .resolved_url(false)
        .map(|_| ())
        .map_err(|error| OnboardingError::Invalid {
            field,
            message: error.to_string(),
        })
}

fn ends_in_responses(endpoint: &str) -> bool {
    url::Url::parse(endpoint).ok().is_some_and(|url| {
        url.path()
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .is_some_and(|segment| segment.eq_ignore_ascii_case("responses"))
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), OnboardingError> {
    let parent = path.parent().ok_or_else(|| OnboardingError::Invalid {
        field: "path",
        message: "destination has no parent".to_owned(),
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| OnboardingError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    use std::io::Write as _;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| OnboardingError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| OnboardingError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

#[derive(Serialize)]
struct StoredConfig<'a> {
    api: StoredApi<'a>,
    agent: StoredAgent<'a>,
    ui: StoredUi<'a>,
}

#[derive(Serialize)]
struct StoredApi<'a> {
    provider: &'a str,
    provider_auth: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    keyring_account: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    responses_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    azure_base_url: Option<&'a str>,
    deployment: &'a str,
    deployment_choices: Vec<&'a str>,
    max_output_tokens: u32,
    reasoning_effort: &'a str,
    transport: &'a str,
    request_timeout_secs: u64,
    stream_idle_timeout_secs: u64,
    max_attempts: u32,
    retry_min_delay_ms: u64,
    retry_max_delay_secs: u64,
    retry_after_cap_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    aws_region: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aws_profile: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    aws_role_arn: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bedrock_endpoint_url: Option<&'a str>,
}

#[derive(Serialize)]
struct StoredAgent<'a> {
    context_mode: &'a str,
    context_budget: u32,
    max_context_budget: u32,
    workspace_root: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_dir: Option<String>,
    instructions_file: &'a str,
}

#[derive(Serialize)]
struct StoredUi<'a> {
    confirm_destructive: bool,
    mouse_enabled: bool,
    language: &'a str,
    onboarding_completed: bool,
    mascot_enabled: bool,
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn saved_config_contains_no_secret_and_is_parseable() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            language: UiLanguage::Russian,
            provider: SetupProvider::Google,
            model: "gemini-test".to_owned(),
            endpoint: String::new(),
            api_key: SecretString::new("never-write-me".to_owned().into()),
            workspace,
            context_budget: 400_000,
            use_case: "Rust".to_owned(),
            ..OnboardingAnswers::default()
        };
        let config_dir = root.path().join("config");
        persist_mascot_preference_to(&config_dir, UiLanguage::English, true, false)?;
        persist_default_context_budget_to(&config_dir, UiLanguage::English, true, 100_000)?;
        let path = persist_to(&answers, &config_dir, None, false)?;
        let encoded = std::fs::read_to_string(path)?;
        assert!(!encoded.contains("never-write-me"));
        assert!(encoded.contains("keyring_account = \"google-primary\""));
        assert!(encoded.contains("language = \"ru\""));
        let _: toml::Value = toml::from_str(&encoded)?;
        assert_eq!(
            load_ui_preferences_from(&config_dir)?,
            Some(UiPreferences {
                language: UiLanguage::Russian,
                onboarding_completed: true,
                mascot_enabled: Some(false),
                default_context_budget: Some(400_000),
            })
        );
        Ok(())
    }

    #[test]
    fn setup_does_not_lock_runtime_picker_to_the_initial_context_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::Google,
            model: "gemini-test".to_owned(),
            api_key: SecretString::new("test-key".to_owned().into()),
            workspace,
            context_budget: 100_000,
            ..OnboardingAnswers::default()
        };

        let path = persist_to(&answers, root.path(), None, false)?;
        let stored: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
        let agent = stored
            .get("agent")
            .and_then(toml::Value::as_table)
            .ok_or("missing agent table")?;

        assert_eq!(
            agent
                .get("context_budget")
                .and_then(toml::Value::as_integer),
            Some(100_000)
        );
        assert_eq!(
            agent
                .get("max_context_budget")
                .and_then(toml::Value::as_integer),
            Some(i64::from(crate::config::MAX_CONTEXT_BUDGET))
        );
        Ok(())
    }

    #[test]
    fn azure_setup_persists_a_base_url_that_resolves_to_responses()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::Azure,
            model: "deployment".to_owned(),
            endpoint: "https://resource.example/openai/v1".to_owned(),
            api_key: SecretString::new("test-key".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        let path = persist_to(&answers, root.path(), None, false)?;
        let encoded = std::fs::read_to_string(path)?;
        let stored: toml::Value = toml::from_str(&encoded)?;
        let api = stored
            .get("api")
            .and_then(toml::Value::as_table)
            .ok_or("missing api table")?;

        assert_eq!(
            api.get("azure_base_url").and_then(toml::Value::as_str),
            Some("https://resource.example/openai/v1")
        );
        assert!(!api.contains_key("responses_url"));
        Ok(())
    }

    #[test]
    fn azure_setup_accepts_an_already_complete_responses_url()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::Azure,
            model: "deployment".to_owned(),
            endpoint: "https://resource.example/openai/v1/responses".to_owned(),
            api_key: SecretString::new("test-key".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        let path = persist_to(&answers, root.path(), None, false)?;
        let encoded = std::fs::read_to_string(path)?;
        let api = toml::from_str::<toml::Value>(&encoded)?
            .get("api")
            .and_then(toml::Value::as_table)
            .cloned()
            .ok_or("missing api table")?;

        assert_eq!(
            api.get("responses_url").and_then(toml::Value::as_str),
            Some("https://resource.example/openai/v1/responses")
        );
        assert!(!api.contains_key("azure_base_url"));
        Ok(())
    }

    #[test]
    fn azure_setup_does_not_duplicate_a_trailing_slash_responses_path()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::Azure,
            model: "deployment".to_owned(),
            endpoint: "https://resource.example/openai/v1/responses/".to_owned(),
            api_key: SecretString::new("test-key".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        let path = persist_to(&answers, root.path(), None, false)?;
        let encoded = std::fs::read_to_string(path)?;
        let api = toml::from_str::<toml::Value>(&encoded)?
            .get("api")
            .and_then(toml::Value::as_table)
            .cloned()
            .ok_or("missing api table")?;

        assert!(api.contains_key("responses_url"));
        assert!(!api.contains_key("azure_base_url"));
        Ok(())
    }

    #[test]
    fn setup_started_from_a_build_folder_defaults_to_the_project_root()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        std::fs::write(root.path().join("Cargo.toml"), "[package]\nname='sample'\n")?;
        let release = root.path().join("target").join("release");
        std::fs::create_dir_all(&release)?;

        assert_eq!(default_workspace_from(&release), root.path());
        Ok(())
    }

    #[test]
    fn skipped_setup_keeps_the_selected_language_for_the_next_launch()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;

        persist_ui_preferences_to(root.path(), UiLanguage::Ukrainian, false)?;

        assert_eq!(
            load_ui_preferences_from(root.path())?,
            Some(UiPreferences {
                language: UiLanguage::Ukrainian,
                onboarding_completed: false,
                mascot_enabled: None,
                default_context_budget: None,
            })
        );
        Ok(())
    }

    #[test]
    fn legacy_ui_preferences_leave_new_overrides_unset() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        std::fs::write(
            root.path().join(PREFERENCES_FILE),
            "version = 1\nlanguage = \"en\"\nonboarding_completed = true\n",
        )?;

        assert_eq!(
            load_ui_preferences_from(root.path())?,
            Some(UiPreferences {
                language: UiLanguage::English,
                onboarding_completed: true,
                mascot_enabled: None,
                default_context_budget: None,
            })
        );
        Ok(())
    }

    #[test]
    fn independent_ui_preferences_survive_unrelated_updates()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        persist_mascot_preference_to(root.path(), UiLanguage::Russian, true, false)?;
        persist_default_context_budget_to(root.path(), UiLanguage::Russian, true, 300_000)?;
        persist_ui_preferences_to(root.path(), UiLanguage::Ukrainian, true)?;

        assert_eq!(
            load_ui_preferences_from(root.path())?,
            Some(UiPreferences {
                language: UiLanguage::Ukrainian,
                onboarding_completed: true,
                mascot_enabled: Some(false),
                default_context_budget: Some(300_000),
            })
        );
        Ok(())
    }

    #[test]
    fn skipped_required_setup_persists_a_loadable_local_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let config_dir = root.path().join("config");
        let mut answers = local_fallback_answers(UiLanguage::Ukrainian);
        answers.workspace.clone_from(&workspace);
        let config_path = persist_to(&answers, &config_dir, Some(root.path()), false)?;

        let config = crate::config::AppConfig::load_from(crate::config::CliArgs {
            config_file: Some(config_path),
            ..crate::config::CliArgs::default()
        })?;

        assert_eq!(config.api.provider, crate::config::ApiProvider::Ollama);
        assert_eq!(
            config.agent.workspace_root,
            std::fs::canonicalize(workspace)?
        );
        assert_eq!(config.ui.language, UiLanguage::Ukrainian);
        assert!(config.ui.onboarding_completed);
        Ok(())
    }

    #[test]
    fn rerun_preserves_advanced_sections_and_manual_instructions()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let config_dir = root.path().join("config");
        std::fs::create_dir(&config_dir)?;
        std::fs::write(
            config_dir.join("config.toml"),
            r#"
[api]
provider = "azure"
azure_base_url = "https://stale.example/openai/v1"
deployment = "old"
auto_pricing = false

[[api.pricing]]
deployment = "private"
input_usd_per_million = 1.0
output_usd_per_million = 2.0

[agent]
context_mode = "stateless"
context_budget = 100000
max_context_budget = 100000
workspace_root = "D:/old"
instructions_file = "D:/old/instructions.md"

[agent.subagents]
enabled = true
max_parallel = 3

[ui]
language = "en"
onboarding_completed = true
mouse_enabled = true

[mcp_servers.docs]
transport = "http"
url = "https://example.test/mcp"
"#,
        )?;
        std::fs::write(
            config_dir.join("instructions.md"),
            "# keep my instructions\n",
        )?;
        let answers = OnboardingAnswers {
            language: UiLanguage::Ukrainian,
            provider: SetupProvider::OpenAi,
            model: "gpt-test".to_owned(),
            endpoint: String::new(),
            api_key: SecretString::new("never-write-me".to_owned().into()),
            workspace,
            context_budget: 400_000,
            use_case: "Rust".to_owned(),
            ..OnboardingAnswers::default()
        };
        let path = persist_to(&answers, &config_dir, None, false)?;
        let encoded = std::fs::read_to_string(path)?;
        assert!(encoded.contains("auto_pricing = false"));
        assert!(encoded.contains("[agent.subagents]"));
        assert!(encoded.contains("[mcp_servers.docs]"));
        assert!(encoded.contains("provider = \"openai\""));
        assert!(!encoded.contains("azure_base_url"));
        assert_eq!(
            std::fs::read_to_string(config_dir.join("instructions.md"))?,
            "# keep my instructions\n"
        );
        Ok(())
    }

    #[test]
    fn bedrock_runtime_setup_persists_explicit_sdk_and_network_settings()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::AwsBedrockRuntime,
            model: "us.anthropic.claude-test-v1:0".to_owned(),
            workspace,
            api_transport: "sse".to_owned(),
            request_timeout_secs: 240,
            stream_idle_timeout_secs: 300,
            max_attempts: 4,
            retry_min_delay_ms: 750,
            retry_max_delay_secs: 25,
            retry_after_cap_secs: 90,
            aws_region: Some("us-east-1".to_owned()),
            aws_profile: Some("decode".to_owned()),
            aws_role_arn: Some("arn:aws:iam::123456789012:role/decode".to_owned()),
            bedrock_endpoint_url: Some(
                "https://bedrock-runtime.us-east-1.amazonaws.com".to_owned(),
            ),
            ..OnboardingAnswers::default()
        };
        let path = persist_to(&answers, root.path(), None, false)?;
        let encoded = std::fs::read_to_string(path)?;

        for expected in [
            "provider = \"bedrock_runtime\"",
            "provider_auth = \"aws_sdk\"",
            "transport = \"sse\"",
            "request_timeout_secs = 240",
            "stream_idle_timeout_secs = 300",
            "max_attempts = 4",
            "retry_min_delay_ms = 750",
            "retry_max_delay_secs = 25",
            "retry_after_cap_secs = 90",
            "aws_region = \"us-east-1\"",
            "aws_profile = \"decode\"",
            "aws_role_arn = \"arn:aws:iam::123456789012:role/decode\"",
        ] {
            assert!(encoded.contains(expected), "missing {expected}");
        }
        assert!(!encoded.contains("keyring_account"));
        let loaded = crate::config::AppConfig::load_from(crate::config::CliArgs {
            config_file: Some(root.path().join("config.toml")),
            ..crate::config::CliArgs::default()
        })?;
        assert_eq!(
            loaded.api.provider,
            crate::config::ApiProvider::AwsBedrockRuntime
        );
        assert_eq!(loaded.api.transport, crate::config::ApiTransport::Sse);
        assert_eq!(loaded.api.request_timeout.as_secs(), 240);
        assert_eq!(loaded.api.stream_idle_timeout.as_secs(), 300);
        assert_eq!(loaded.api.max_attempts, 4);
        assert_eq!(
            loaded.api.bedrock_runtime.region.as_deref(),
            Some("us-east-1")
        );
        Ok(())
    }

    #[test]
    fn corrupt_ui_preferences_fail_closed_with_their_path() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = TempDir::new()?;
        std::fs::write(root.path().join(PREFERENCES_FILE), "not = [valid")?;
        let error = match load_ui_preferences_from(root.path()) {
            Ok(_) => return Err("corrupt preferences were silently accepted".into()),
            Err(error) => error,
        };
        let rendered = error.to_string();
        assert!(rendered.contains(PREFERENCES_FILE));
        assert!(rendered.contains("invalid"));
        Ok(())
    }

    #[test]
    fn explicit_short_config_argument_suppresses_implicit_setup() {
        assert!(explicit_config_requested([
            "-c".into(),
            "custom.toml".into()
        ]));
        assert!(explicit_config_requested(["-ccustom.toml".into()]));
    }

    #[test]
    fn invalid_implicit_responses_url_reopens_setup() {
        let error = ConfigError::InvalidValue {
            field: "api.responses_url",
            message: "relative URL without a base".to_owned(),
        };

        assert!(should_offer_with_context(&error, true, []));
        assert!(!should_offer_with_context(
            &error,
            true,
            ["--config=broken.toml".into()]
        ));
    }

    #[test]
    fn invalid_endpoint_is_rejected_before_setup_writes_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let config_dir = root.path().join("config");
        let answers = OnboardingAnswers {
            provider: SetupProvider::Azure,
            endpoint: "http://example.test/openai/v1/responses".to_owned(),
            api_key: SecretString::new("secret".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        assert!(matches!(
            persist_to(&answers, &config_dir, None, false),
            Err(OnboardingError::Invalid {
                field: "endpoint",
                ..
            })
        ));
        assert!(!config_dir.join("config.toml").exists());
        assert!(!config_dir.join("instructions.md").exists());
        assert!(!config_dir.join(PREFERENCES_FILE).exists());
        Ok(())
    }

    #[test]
    fn invalid_bedrock_override_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::AwsBedrockRuntime,
            model: "us.anthropic.claude-test-v1:0".to_owned(),
            workspace,
            bedrock_endpoint_url: Some("file:///tmp/credentials".to_owned()),
            ..OnboardingAnswers::default()
        };

        assert!(matches!(
            validate_answers(&answers),
            Err(OnboardingError::Invalid {
                field: "api.bedrock_endpoint_url",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn persistence_rejects_unbounded_use_case_text() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::OpenAi,
            model: "gpt-test".to_owned(),
            api_key: SecretString::new("secret".to_owned().into()),
            workspace,
            use_case: "x".repeat(4_097),
            ..OnboardingAnswers::default()
        };

        assert!(matches!(
            validate_answers(&answers),
            Err(OnboardingError::Invalid {
                field: "use_case",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn persistence_rejects_a_blank_provider_secret() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::OpenAi,
            model: "gpt-test".to_owned(),
            api_key: SecretString::new("   ".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        assert!(matches!(
            validate_answers(&answers),
            Err(OnboardingError::Invalid {
                field: "api_key",
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn invalid_existing_config_does_not_create_setup_artifacts()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let workspace = root.path().join("workspace");
        let config_dir = root.path().join("config");
        std::fs::create_dir(&workspace)?;
        std::fs::create_dir(&config_dir)?;
        std::fs::write(config_dir.join("config.toml"), "not = [valid")?;
        let answers = OnboardingAnswers {
            provider: SetupProvider::OpenAi,
            model: "gpt-test".to_owned(),
            api_key: SecretString::new("secret".to_owned().into()),
            workspace,
            ..OnboardingAnswers::default()
        };

        assert!(persist_to(&answers, &config_dir, None, false).is_err());
        assert!(!config_dir.join("instructions.md").exists());
        assert!(!config_dir.join(PREFERENCES_FILE).exists());
        Ok(())
    }
}
