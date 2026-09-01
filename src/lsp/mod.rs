mod client;

use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::{
    api::FunctionToolDefinition, error::ConfigError, notice::UiNotice, privacy::PrivacyShield,
};

use self::client::{LspClient, LspDocument, LspQuery};

pub const LSP_STATUS_TOOL: &str = "lsp_status";
pub const LSP_DIAGNOSTICS_TOOL: &str = "lsp_diagnostics";
pub const LSP_DOCUMENT_SYMBOLS_TOOL: &str = "lsp_document_symbols";
pub const LSP_WORKSPACE_SYMBOLS_TOOL: &str = "lsp_workspace_symbols";
pub const LSP_DEFINITION_TOOL: &str = "lsp_definition";
pub const LSP_REFERENCES_TOOL: &str = "lsp_references";
pub const LSP_HOVER_TOOL: &str = "lsp_hover";

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(12);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const DEFAULT_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_RESULT_BYTES: usize = 128 * 1024;
const DEFAULT_MAX_DIAGNOSTICS: usize = 2_000;
const MAX_LSP_SERVERS: usize = 32;
const MAX_LSP_ARGS: usize = 64;
const MAX_LSP_ARG_BYTES: usize = 4_096;
const MAX_LSP_MARKERS: usize = 64;
const MAX_LSP_EXTENSIONS: usize = 64;
const MAX_LSP_DOCUMENT_BYTES: usize = 4 * 1024 * 1024;
const MAX_LSP_QUERY_BYTES: usize = 4_096;
const MAX_NORMALIZED_ITEMS: usize = 256;
const MAX_TEXT_FIELD_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspConfig {
    pub enabled: bool,
    pub startup_timeout: Duration,
    pub request_timeout: Duration,
    pub max_message_bytes: usize,
    pub max_result_bytes: usize,
    pub max_diagnostics: usize,
    pub servers: Vec<LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_diagnostics: DEFAULT_MAX_DIAGNOSTICS,
            servers: Vec::new(),
        }
    }
}

impl LspConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.startup_timeout.is_zero() || self.startup_timeout > Duration::from_secs(120) {
            return Err(invalid_config(
                "lsp.startup_timeout_secs",
                "must be between 1 and 120 seconds",
            ));
        }
        if self.request_timeout.is_zero() || self.request_timeout > Duration::from_secs(120) {
            return Err(invalid_config(
                "lsp.request_timeout_secs",
                "must be between 1 and 120 seconds",
            ));
        }
        if !(64 * 1024..=16 * 1024 * 1024).contains(&self.max_message_bytes) {
            return Err(invalid_config(
                "lsp.max_message_bytes",
                "must be between 65536 and 16777216",
            ));
        }
        if !(4 * 1024..=1024 * 1024).contains(&self.max_result_bytes) {
            return Err(invalid_config(
                "lsp.max_result_bytes",
                "must be between 4096 and 1048576",
            ));
        }
        if !(1..=20_000).contains(&self.max_diagnostics) {
            return Err(invalid_config(
                "lsp.max_diagnostics",
                "must be between 1 and 20000",
            ));
        }
        if self.servers.len() > MAX_LSP_SERVERS {
            return Err(invalid_config(
                "lsp.servers",
                format!("must contain at most {MAX_LSP_SERVERS} servers"),
            ));
        }

        let mut names = std::collections::BTreeSet::new();
        for server in &self.servers {
            server.validate()?;
            if !names.insert(server.name.clone()) {
                return Err(invalid_config(
                    "lsp.servers.name",
                    format!("duplicate server name {:?}", server.name),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LspServerConfig {
    pub name: String,
    pub enabled: bool,
    pub required: bool,
    pub auto_start: bool,
    pub command: String,
    pub args: Vec<String>,
    pub language_id: String,
    pub extensions: Vec<String>,
    pub root_markers: Vec<String>,
}

impl LspServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_small_text("lsp.servers.name", &self.name, 128)?;
        validate_small_text("lsp.servers.command", &self.command, 4_096)?;
        validate_small_text("lsp.servers.language_id", &self.language_id, 128)?;
        if self.args.len() > MAX_LSP_ARGS {
            return Err(invalid_config(
                "lsp.servers.args",
                format!("must contain at most {MAX_LSP_ARGS} arguments"),
            ));
        }
        for argument in &self.args {
            validate_small_text("lsp.servers.args", argument, MAX_LSP_ARG_BYTES)?;
        }
        if self.extensions.len() > MAX_LSP_EXTENSIONS {
            return Err(invalid_config(
                "lsp.servers.extensions",
                format!("must contain at most {MAX_LSP_EXTENSIONS} extensions"),
            ));
        }
        for extension in &self.extensions {
            validate_small_text("lsp.servers.extensions", extension, 64)?;
            if !extension.starts_with('.') || extension.contains('/') || extension.contains('\\') {
                return Err(invalid_config(
                    "lsp.servers.extensions",
                    "every extension must start with '.' and contain no path separator",
                ));
            }
        }
        if self.root_markers.len() > MAX_LSP_MARKERS {
            return Err(invalid_config(
                "lsp.servers.root_markers",
                format!("must contain at most {MAX_LSP_MARKERS} markers"),
            ));
        }
        for marker in &self.root_markers {
            validate_small_text("lsp.servers.root_markers", marker, 512)?;
            let path = Path::new(marker);
            if path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        Component::ParentDir | Component::RootDir | Component::Prefix(_)
                    )
                })
            {
                return Err(invalid_config(
                    "lsp.servers.root_markers",
                    "markers must be relative paths without '..'",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn supports_path(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| format!(".{}", value.to_ascii_lowercase()));
        extension.is_some_and(|extension| {
            self.extensions
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(&extension))
        })
    }
}

fn invalid_config(field: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidValue {
        field,
        message: message.into(),
    }
}

fn validate_small_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), ConfigError> {
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(invalid_config(
            field,
            format!("must be non-empty, NUL-free, and at most {max_bytes} bytes"),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LspConnectionState {
    Disabled,
    NotDetected,
    Disconnected,
    Starting,
    Connected,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspServerSnapshot {
    pub name: String,
    pub language_id: String,
    pub runtime_available: bool,
    pub enabled: bool,
    pub required: bool,
    pub auto_start: bool,
    pub detected: bool,
    pub state: LspConnectionState,
    pub diagnostic_count: usize,
    pub notice: UiNotice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LspDiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    Unknown,
}

impl LspDiagnosticSeverity {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Hint => "hint",
            Self::Unknown => "unknown",
        }
    }

    const fn from_lsp(value: Option<u64>) -> Self {
        match value {
            Some(1) => Self::Error,
            Some(2) => Self::Warning,
            Some(3) => Self::Information,
            Some(4) => Self::Hint,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspDiagnostic {
    pub server: String,
    pub path: String,
    pub line: u64,
    pub column: u64,
    pub end_line: u64,
    pub end_column: u64,
    pub severity: LspDiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspCallOutput {
    pub content: String,
    pub truncated: bool,
}

#[derive(Debug, Error)]
pub enum LspError {
    #[error("LSP is disabled globally in trusted configuration")]
    RuntimeDisabled,
    #[error("LSP server {server:?} is not configured")]
    UnknownServer { server: String },
    #[error("no configured LSP server matches {path:?}")]
    NoMatchingServer { path: String },
    #[error("more than one LSP server is available; select one explicitly: {servers}")]
    AmbiguousServer { servers: String },
    #[error("LSP server {server:?} is disabled for this run")]
    ServerDisabled { server: String },
    #[error("LSP server {server:?} was not detected in this workspace")]
    ServerNotDetected { server: String },
    #[error("LSP server {server:?} is not connected: {message}")]
    NotConnected { server: String, message: String },
    #[error("LSP server {server:?} failed to start: {message}")]
    Startup { server: String, message: String },
    #[error("LSP {operation} on {server:?} timed out after {secs}s")]
    Timeout {
        server: String,
        operation: String,
        secs: u64,
    },
    #[error("LSP protocol error from {server:?}: {message}")]
    Protocol { server: String, message: String },
    #[error("LSP input is invalid: {0}")]
    InvalidInput(String),
    #[error("LSP path {path:?} is outside the workspace")]
    UnsafePath { path: String },
    #[error("failed to access LSP path {path:?}: {source}")]
    PathIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("LSP document {path:?} is not valid UTF-8")]
    InvalidUtf8 { path: PathBuf },
    #[error("LSP document {path:?} exceeds the {limit} byte limit")]
    DocumentTooLarge { path: PathBuf, limit: usize },
    #[error("LSP runtime channel for {server:?} closed unexpectedly")]
    ChannelClosed { server: String },
    #[error("LSP {operation} on {server:?} was cancelled")]
    Cancelled { server: String, operation: String },
}

#[derive(Debug, Default)]
struct SharedCatalog {
    snapshots: BTreeMap<String, LspServerSnapshot>,
    diagnostics: BTreeMap<(String, String), Vec<LspDiagnostic>>,
}

#[derive(Debug)]
pub(crate) struct LspShared {
    root: PathBuf,
    privacy: Option<PrivacyShield>,
    max_diagnostics: usize,
    catalog: Mutex<SharedCatalog>,
}

impl LspShared {
    fn new(
        root: PathBuf,
        privacy: Option<PrivacyShield>,
        max_diagnostics: usize,
        snapshots: Vec<LspServerSnapshot>,
    ) -> Self {
        let snapshots = snapshots
            .into_iter()
            .map(|snapshot| (snapshot.name.clone(), snapshot))
            .collect();
        Self {
            root,
            privacy,
            max_diagnostics,
            catalog: Mutex::new(SharedCatalog {
                snapshots,
                diagnostics: BTreeMap::new(),
            }),
        }
    }

    pub(crate) fn set_state(&self, server: &str, state: LspConnectionState, notice: UiNotice) {
        let Ok(mut catalog) = self.catalog.lock() else {
            tracing::error!(server, "LSP snapshot lock was poisoned");
            return;
        };
        if let Some(snapshot) = catalog.snapshots.get_mut(server) {
            snapshot.state = state;
            snapshot.notice = notice;
        }
    }

    fn set_enabled(&self, server: &str, enabled: bool, detected: bool) {
        let Ok(mut catalog) = self.catalog.lock() else {
            tracing::error!(server, "LSP snapshot lock was poisoned");
            return;
        };
        if let Some(snapshot) = catalog.snapshots.get_mut(server) {
            snapshot.enabled = enabled;
            snapshot.state = if !enabled {
                LspConnectionState::Disabled
            } else if !detected {
                LspConnectionState::NotDetected
            } else {
                LspConnectionState::Disconnected
            };
            snapshot.notice = UiNotice::None;
        }
    }

    fn insert_snapshot(&self, snapshot: LspServerSnapshot) {
        let Ok(mut catalog) = self.catalog.lock() else {
            tracing::error!(server = %snapshot.name, "LSP snapshot lock was poisoned");
            return;
        };
        catalog.snapshots.insert(snapshot.name.clone(), snapshot);
    }

    pub(crate) fn publish_diagnostics(&self, server: &str, params: &Value) {
        let Some(uri) = params.get("uri").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = relative_uri_path(&self.root, uri) else {
            tracing::warn!(server, "ignored LSP diagnostics outside the workspace");
            return;
        };
        if self
            .privacy
            .as_ref()
            .is_some_and(|shield| shield.check_relative(Path::new(&path), false).is_err())
        {
            tracing::warn!(server, path, "ignored LSP diagnostics for a sensitive path");
            return;
        }
        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| normalize_diagnostic(server, &path, item))
                    .take(self.max_diagnostics)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Ok(mut catalog) = self.catalog.lock() else {
            tracing::error!(server, "LSP diagnostic lock was poisoned");
            return;
        };
        catalog
            .diagnostics
            .insert((server.to_owned(), path), diagnostics);
        trim_diagnostics(&mut catalog, self.max_diagnostics);
        refresh_diagnostic_counts(&mut catalog);
    }

    fn clear_server_diagnostics(&self, server: &str) {
        let Ok(mut catalog) = self.catalog.lock() else {
            tracing::error!(server, "LSP diagnostic lock was poisoned");
            return;
        };
        catalog
            .diagnostics
            .retain(|(diagnostic_server, _), _| diagnostic_server != server);
        refresh_diagnostic_counts(&mut catalog);
    }

    fn snapshots(&self) -> Vec<LspServerSnapshot> {
        self.catalog
            .lock()
            .map(|catalog| catalog.snapshots.values().cloned().collect())
            .unwrap_or_else(|_| {
                tracing::error!("LSP snapshot lock was poisoned");
                Vec::new()
            })
    }

    fn diagnostics(&self) -> Vec<LspDiagnostic> {
        self.catalog
            .lock()
            .map(|catalog| {
                catalog
                    .diagnostics
                    .values()
                    .flatten()
                    .filter(|diagnostic| {
                        self.privacy.as_ref().is_none_or(|shield| {
                            shield.allows_relative(Path::new(&diagnostic.path), false)
                        })
                    })
                    .take(self.max_diagnostics)
                    .cloned()
                    .collect()
            })
            .unwrap_or_else(|_| {
                tracing::error!("LSP diagnostic lock was poisoned");
                Vec::new()
            })
    }
}

fn trim_diagnostics(catalog: &mut SharedCatalog, limit: usize) {
    let mut remaining = limit;
    for diagnostics in catalog.diagnostics.values_mut() {
        if remaining == 0 {
            diagnostics.clear();
        } else if diagnostics.len() > remaining {
            diagnostics.truncate(remaining);
            remaining = 0;
        } else {
            remaining = remaining.saturating_sub(diagnostics.len());
        }
    }
    catalog.diagnostics.retain(|_, values| !values.is_empty());
}

fn refresh_diagnostic_counts(catalog: &mut SharedCatalog) {
    for snapshot in catalog.snapshots.values_mut() {
        snapshot.diagnostic_count = 0;
    }
    let counts = catalog
        .diagnostics
        .iter()
        .map(|((server, _), diagnostics)| (server.clone(), diagnostics.len()))
        .collect::<Vec<_>>();
    for (server, count) in counts {
        if let Some(snapshot) = catalog.snapshots.get_mut(&server) {
            snapshot.diagnostic_count = snapshot.diagnostic_count.saturating_add(count);
        }
    }
}

struct RuntimeServer {
    config: LspServerConfig,
    detected: bool,
    client: Option<LspClient>,
}

pub struct LspManager {
    config: LspConfig,
    root: PathBuf,
    privacy: Option<PrivacyShield>,
    shared: Arc<LspShared>,
    servers: BTreeMap<String, RuntimeServer>,
}

impl LspManager {
    pub fn new(config: LspConfig, workspace_root: &Path) -> Result<Self, ConfigError> {
        let privacy = PrivacyShield::load_project_only(workspace_root).ok();
        Self::new_inner(config, workspace_root, privacy)
    }

    pub(crate) fn new_with_privacy(
        config: LspConfig,
        workspace_root: &Path,
        privacy: PrivacyShield,
    ) -> Result<Self, ConfigError> {
        Self::new_inner(config, workspace_root, Some(privacy))
    }

    fn new_inner(
        config: LspConfig,
        workspace_root: &Path,
        privacy: Option<PrivacyShield>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let root = dunce::canonicalize(workspace_root).map_err(|source| ConfigError::PathIo {
            field: "agent.workspace_root",
            path: workspace_root.to_path_buf(),
            source,
        })?;
        let detections = detect_servers(&root, &config.servers);
        let snapshots = config
            .servers
            .iter()
            .map(|server| {
                let detected = detections.get(&server.name).copied().unwrap_or(false);
                LspServerSnapshot {
                    name: server.name.clone(),
                    language_id: server.language_id.clone(),
                    runtime_available: config.enabled,
                    enabled: server.enabled,
                    required: server.required,
                    auto_start: server.auto_start,
                    detected,
                    state: if !config.enabled || !server.enabled {
                        LspConnectionState::Disabled
                    } else if !detected {
                        LspConnectionState::NotDetected
                    } else {
                        LspConnectionState::Disconnected
                    },
                    diagnostic_count: 0,
                    notice: UiNotice::None,
                }
            })
            .collect::<Vec<_>>();
        let shared = Arc::new(LspShared::new(
            root.clone(),
            privacy.clone(),
            config.max_diagnostics,
            snapshots,
        ));
        let servers = config
            .servers
            .iter()
            .cloned()
            .map(|server| {
                let detected = detections.get(&server.name).copied().unwrap_or(false);
                (
                    server.name.clone(),
                    RuntimeServer {
                        config: server,
                        detected,
                        client: None,
                    },
                )
            })
            .collect();
        Ok(Self {
            config,
            root,
            privacy,
            shared,
            servers,
        })
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<LspServerSnapshot> {
        self.shared.snapshots()
    }

    #[must_use]
    pub fn diagnostics(&self) -> Vec<LspDiagnostic> {
        self.shared.diagnostics()
    }

    pub fn privacy_reloaded(&self) {
        let Ok(mut catalog) = self.shared.catalog.lock() else {
            tracing::error!("LSP diagnostic lock was poisoned during privacy reload");
            return;
        };
        catalog.diagnostics.retain(|(_, path), _| {
            self.privacy
                .as_ref()
                .is_none_or(|shield| shield.allows_relative(Path::new(path), false))
        });
        refresh_diagnostic_counts(&mut catalog);
    }

    #[must_use]
    pub fn function_definitions(&self) -> Vec<FunctionToolDefinition> {
        if !self.config.enabled || self.servers.is_empty() {
            return Vec::new();
        }
        lsp_function_definitions()
    }

    pub async fn start(&mut self) -> Result<(), LspError> {
        if !self.config.enabled {
            return Ok(());
        }
        let names = self
            .servers
            .values()
            .filter(|server| server.config.enabled && server.config.auto_start && server.detected)
            .map(|server| server.config.name.clone())
            .collect::<Vec<_>>();
        for name in names {
            if let Err(error) = self.connect(&name).await {
                let required = self
                    .servers
                    .get(&name)
                    .is_some_and(|server| server.config.required);
                tracing::warn!(server = %name, error = %error, "LSP auto-start failed");
                if required {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub async fn connect(&mut self, name: &str) -> Result<(), LspError> {
        if !self.config.enabled {
            return Err(LspError::RuntimeDisabled);
        }
        let (server_config, detected) = self
            .servers
            .get(name)
            .map(|server| (server.config.clone(), server.detected))
            .ok_or_else(|| LspError::UnknownServer {
                server: name.to_owned(),
            })?;
        if !server_config.enabled {
            return Err(LspError::ServerDisabled {
                server: name.to_owned(),
            });
        }
        if !detected {
            return Err(LspError::ServerNotDetected {
                server: name.to_owned(),
            });
        }
        self.disconnect(name).await;
        self.shared
            .set_state(name, LspConnectionState::Starting, UiNotice::LspStarting);
        match LspClient::connect(
            server_config,
            self.root.clone(),
            self.config.startup_timeout,
            self.config.request_timeout,
            self.config.max_message_bytes,
            Arc::clone(&self.shared),
        )
        .await
        {
            Ok(client) => {
                if let Some(server) = self.servers.get_mut(name) {
                    server.client = Some(client);
                }
                self.shared
                    .set_state(name, LspConnectionState::Connected, UiNotice::LspReady);
                Ok(())
            }
            Err(error) => {
                self.shared.set_state(
                    name,
                    LspConnectionState::Error,
                    UiNotice::external(error.to_string()),
                );
                Err(error)
            }
        }
    }

    pub async fn disconnect(&mut self, name: &str) {
        let Some(server) = self.servers.get_mut(name) else {
            return;
        };
        if let Some(client) = server.client.take() {
            client.shutdown(self.config.startup_timeout).await;
        }
        self.shared.clear_server_diagnostics(name);
        let state = if !self.config.enabled || !server.config.enabled {
            LspConnectionState::Disabled
        } else if !server.detected {
            LspConnectionState::NotDetected
        } else {
            LspConnectionState::Disconnected
        };
        self.shared.set_state(name, state, UiNotice::Stopped);
    }

    pub async fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), LspError> {
        if enabled && !self.config.enabled {
            return Err(LspError::RuntimeDisabled);
        }
        let detected = self
            .servers
            .get(name)
            .map(|server| server.detected)
            .ok_or_else(|| LspError::UnknownServer {
                server: name.to_owned(),
            })?;
        if !enabled {
            self.disconnect(name).await;
        }
        if let Some(server) = self.servers.get_mut(name) {
            server.config.enabled = enabled;
        }
        self.shared.set_enabled(name, enabled, detected);
        if enabled && detected {
            self.connect(name).await?;
        }
        Ok(())
    }

    /// Adds one user-managed language server to the live catalog. No process
    /// starts during save; auto-start remains a next-launch preference.
    pub fn add_server(&mut self, server: LspServerConfig) -> Result<(), ConfigError> {
        self.validate_add(&server)?;
        let detected = detect_servers(&self.root, std::slice::from_ref(&server))
            .get(&server.name)
            .copied()
            .unwrap_or(false);
        let state = if !self.config.enabled || !server.enabled {
            LspConnectionState::Disabled
        } else if detected {
            LspConnectionState::Disconnected
        } else {
            LspConnectionState::NotDetected
        };
        self.shared.insert_snapshot(LspServerSnapshot {
            name: server.name.clone(),
            language_id: server.language_id.clone(),
            runtime_available: self.config.enabled,
            enabled: server.enabled,
            required: server.required,
            auto_start: server.auto_start,
            detected,
            state,
            diagnostic_count: 0,
            notice: UiNotice::None,
        });
        self.config.servers.push(server.clone());
        self.servers.insert(
            server.name.clone(),
            RuntimeServer {
                config: server,
                detected,
                client: None,
            },
        );
        Ok(())
    }

    pub fn validate_add(&self, server: &LspServerConfig) -> Result<(), ConfigError> {
        server.validate()?;
        if self.servers.contains_key(&server.name) {
            return Err(invalid_config(
                "lsp.servers.name",
                format!("duplicate server name {:?}", server.name),
            ));
        }
        if self.servers.len() >= MAX_LSP_SERVERS {
            return Err(invalid_config(
                "lsp.servers",
                format!("must contain at most {MAX_LSP_SERVERS} servers"),
            ));
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let names = self.servers.keys().cloned().collect::<Vec<_>>();
        for name in names {
            self.disconnect(&name).await;
        }
    }

    pub async fn call(
        &mut self,
        function: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> Result<LspCallOutput, LspError> {
        if !self.config.enabled {
            return Err(LspError::RuntimeDisabled);
        }
        match function {
            LSP_STATUS_TOOL => {
                let _: EmptyArguments = parse_arguments(arguments)?;
                return self.output(json!({ "servers": self.snapshots() }));
            }
            LSP_DIAGNOSTICS_TOOL => {
                let arguments: DiagnosticsArguments = parse_arguments(arguments)?;
                let diagnostics = self
                    .diagnostics()
                    .into_iter()
                    .filter(|diagnostic| {
                        arguments
                            .path
                            .as_deref()
                            .is_none_or(|path| diagnostic.path == path.replace('\\', "/"))
                            && arguments.severity.as_deref().is_none_or(|severity| {
                                diagnostic.severity.label().eq_ignore_ascii_case(severity)
                            })
                    })
                    .collect::<Vec<_>>();
                return self.output(json!({ "diagnostics": diagnostics_to_json(&diagnostics) }));
            }
            _ => {}
        }

        let invocation = parse_invocation(function, arguments)?;
        let server_name = self.select_server(invocation.server(), invocation.path())?;
        let needs_connect = self
            .servers
            .get(&server_name)
            .is_none_or(|server| server.client.is_none());
        if needs_connect {
            self.connect(&server_name).await?;
        }
        let query = self.build_query(&server_name, invocation).await?;
        let client = self
            .servers
            .get(&server_name)
            .and_then(|server| server.client.as_ref())
            .ok_or_else(|| LspError::NotConnected {
                server: server_name.clone(),
                message: "connection disappeared during query setup".to_owned(),
            })?;
        let raw = client.query(query.clone(), cancel).await?;
        let normalized = normalize_query_result(
            &self.root,
            self.privacy.as_ref(),
            &server_name,
            &query,
            &raw,
        );
        self.output(json!({
            "server": server_name,
            "operation": query.operation,
            "result": normalized,
        }))
    }

    fn select_server(
        &self,
        explicit: Option<&str>,
        path: Option<&str>,
    ) -> Result<String, LspError> {
        if let Some(name) = explicit {
            let server = self
                .servers
                .get(name)
                .ok_or_else(|| LspError::UnknownServer {
                    server: name.to_owned(),
                })?;
            if !server.config.enabled {
                return Err(LspError::ServerDisabled {
                    server: name.to_owned(),
                });
            }
            if !server.detected {
                return Err(LspError::ServerNotDetected {
                    server: name.to_owned(),
                });
            }
            return Ok(name.to_owned());
        }

        let candidates = self
            .servers
            .values()
            .filter(|server| server.config.enabled && server.detected)
            .filter(|server| path.is_none_or(|path| server.config.supports_path(Path::new(path))))
            .map(|server| server.config.name.clone())
            .collect::<Vec<_>>();
        match candidates.as_slice() {
            [name] => Ok(name.clone()),
            [] => Err(LspError::NoMatchingServer {
                path: path.unwrap_or("<workspace>").to_owned(),
            }),
            _ => Err(LspError::AmbiguousServer {
                servers: candidates.join(", "),
            }),
        }
    }

    async fn build_query(
        &self,
        server_name: &str,
        invocation: LspInvocation,
    ) -> Result<LspQuery, LspError> {
        let server = self
            .servers
            .get(server_name)
            .ok_or_else(|| LspError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        match invocation {
            LspInvocation::WorkspaceSymbols { query, .. } => Ok(LspQuery::workspace_symbols(query)),
            LspInvocation::DocumentSymbols { path, .. } => {
                let document = self
                    .load_document(&path, &server.config.language_id)
                    .await?;
                Ok(LspQuery::document_symbols(document))
            }
            LspInvocation::Definition {
                path, line, column, ..
            } => {
                let document = self
                    .load_document(&path, &server.config.language_id)
                    .await?;
                Ok(LspQuery::definition(document, line, column))
            }
            LspInvocation::References {
                path,
                line,
                column,
                include_declaration,
                ..
            } => {
                let document = self
                    .load_document(&path, &server.config.language_id)
                    .await?;
                Ok(LspQuery::references(
                    document,
                    line,
                    column,
                    include_declaration,
                ))
            }
            LspInvocation::Hover {
                path, line, column, ..
            } => {
                let document = self
                    .load_document(&path, &server.config.language_id)
                    .await?;
                Ok(LspQuery::hover(document, line, column))
            }
        }
    }

    async fn load_document(
        &self,
        requested: &str,
        language_id: &str,
    ) -> Result<LspDocument, LspError> {
        if requested.len() > crate::tools::MAX_MODEL_PATH_BYTES || requested.contains('\0') {
            return Err(LspError::InvalidInput(
                "path is empty, contains NUL, or exceeds the path limit".to_owned(),
            ));
        }
        let requested_path = Path::new(requested);
        if requested_path.is_absolute()
            || requested_path.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(LspError::UnsafePath {
                path: requested.to_owned(),
            });
        }
        if self
            .privacy
            .as_ref()
            .is_some_and(|shield| shield.check_relative(requested_path, false).is_err())
        {
            return Err(LspError::UnsafePath {
                path: format!("{requested} (blocked by Privacy Shield)"),
            });
        }
        let candidate = self.root.join(requested_path);
        let resolved = tokio::fs::canonicalize(&candidate)
            .await
            .map_err(|source| LspError::PathIo {
                path: candidate.clone(),
                source,
            })?;
        let resolved = dunce::simplified(&resolved).to_path_buf();
        if !resolved.starts_with(&self.root) || !resolved.is_file() {
            return Err(LspError::UnsafePath {
                path: requested.to_owned(),
            });
        }
        let resolved_relative =
            resolved
                .strip_prefix(&self.root)
                .map_err(|_| LspError::UnsafePath {
                    path: requested.to_owned(),
                })?;
        if self
            .privacy
            .as_ref()
            .is_some_and(|shield| shield.check_relative(resolved_relative, false).is_err())
        {
            return Err(LspError::UnsafePath {
                path: format!("{requested} (resolved target blocked by Privacy Shield)"),
            });
        }
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|source| LspError::PathIo {
                path: resolved.clone(),
                source,
            })?;
        if bytes.len() > MAX_LSP_DOCUMENT_BYTES {
            return Err(LspError::DocumentTooLarge {
                path: resolved,
                limit: MAX_LSP_DOCUMENT_BYTES,
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| LspError::InvalidUtf8 {
            path: resolved.clone(),
        })?;
        let uri = Url::from_file_path(&resolved)
            .map_err(|()| LspError::InvalidInput("could not encode file URI".to_owned()))?
            .to_string();
        Ok(LspDocument {
            uri,
            language_id: language_id.to_owned(),
            text,
        })
    }

    fn output(&self, value: Value) -> Result<LspCallOutput, LspError> {
        let serialized = serde_json::to_string(&value).map_err(|error| LspError::Protocol {
            server: "builtin:lsp".to_owned(),
            message: format!("could not serialize normalized result: {error}"),
        })?;
        if serialized.len() <= self.config.max_result_bytes {
            return Ok(LspCallOutput {
                content: serialized,
                truncated: false,
            });
        }
        let mut preview_limit = self.config.max_result_bytes.saturating_sub(128);
        loop {
            let preview = truncate_utf8(&serialized, preview_limit);
            let content = serde_json::to_string(&json!({
                "truncated": true,
                "preview": preview,
                "original_bytes": serialized.len(),
            }))
            .map_err(|error| LspError::Protocol {
                server: "builtin:lsp".to_owned(),
                message: format!("could not serialize truncated result: {error}"),
            })?;
            if content.len() <= self.config.max_result_bytes {
                return Ok(LspCallOutput {
                    content,
                    truncated: true,
                });
            }
            let overflow = content.len().saturating_sub(self.config.max_result_bytes);
            preview_limit = preview.len().saturating_sub(overflow.max(1));
        }
    }
}

fn detect_servers(root: &Path, servers: &[LspServerConfig]) -> BTreeMap<String, bool> {
    let mut detected = servers
        .iter()
        .map(|server| {
            let marker_found = server
                .root_markers
                .iter()
                .any(|marker| root.join(marker).exists());
            let unconditional = server.root_markers.is_empty() && server.extensions.is_empty();
            (server.name.clone(), marker_found || unconditional)
        })
        .collect::<BTreeMap<_, _>>();
    if detected.values().all(|value| *value) {
        return detected;
    }
    for entry in ignore::WalkBuilder::new(root)
        .follow_links(false)
        .standard_filters(true)
        .max_depth(Some(12))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .take(50_000)
    {
        for server in servers {
            if !detected.get(&server.name).copied().unwrap_or(false)
                && server.configured_extension_matches(entry.path())
            {
                detected.insert(server.name.clone(), true);
            }
        }
        if detected.values().all(|value| *value) {
            break;
        }
    }
    detected
}

impl LspServerConfig {
    fn configured_extension_matches(&self, path: &Path) -> bool {
        !self.extensions.is_empty() && self.supports_path(path)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArguments {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticsArguments {
    path: Option<String>,
    severity: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentArguments {
    path: String,
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceSymbolsArguments {
    query: String,
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PositionArguments {
    path: String,
    line: u64,
    column: u64,
    server: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferencesArguments {
    path: String,
    line: u64,
    column: u64,
    include_declaration: bool,
    server: Option<String>,
}

enum LspInvocation {
    DocumentSymbols {
        path: String,
        server: Option<String>,
    },
    WorkspaceSymbols {
        query: String,
        server: Option<String>,
    },
    Definition {
        path: String,
        line: u64,
        column: u64,
        server: Option<String>,
    },
    References {
        path: String,
        line: u64,
        column: u64,
        include_declaration: bool,
        server: Option<String>,
    },
    Hover {
        path: String,
        line: u64,
        column: u64,
        server: Option<String>,
    },
}

impl LspInvocation {
    fn server(&self) -> Option<&str> {
        match self {
            Self::DocumentSymbols { server, .. }
            | Self::WorkspaceSymbols { server, .. }
            | Self::Definition { server, .. }
            | Self::References { server, .. }
            | Self::Hover { server, .. } => server.as_deref(),
        }
    }

    fn path(&self) -> Option<&str> {
        match self {
            Self::DocumentSymbols { path, .. }
            | Self::Definition { path, .. }
            | Self::References { path, .. }
            | Self::Hover { path, .. } => Some(path),
            Self::WorkspaceSymbols { .. } => None,
        }
    }
}

fn parse_invocation(function: &str, arguments: &str) -> Result<LspInvocation, LspError> {
    match function {
        LSP_DOCUMENT_SYMBOLS_TOOL => {
            let arguments: DocumentArguments = parse_arguments(arguments)?;
            validate_path_argument(&arguments.path)?;
            Ok(LspInvocation::DocumentSymbols {
                path: arguments.path,
                server: arguments.server,
            })
        }
        LSP_WORKSPACE_SYMBOLS_TOOL => {
            let arguments: WorkspaceSymbolsArguments = parse_arguments(arguments)?;
            if arguments.query.is_empty() || arguments.query.len() > MAX_LSP_QUERY_BYTES {
                return Err(LspError::InvalidInput(format!(
                    "workspace symbol query must be 1..={MAX_LSP_QUERY_BYTES} bytes"
                )));
            }
            Ok(LspInvocation::WorkspaceSymbols {
                query: arguments.query,
                server: arguments.server,
            })
        }
        LSP_DEFINITION_TOOL | LSP_HOVER_TOOL => {
            let arguments: PositionArguments = parse_arguments(arguments)?;
            validate_position(&arguments.path, arguments.line, arguments.column)?;
            if function == LSP_DEFINITION_TOOL {
                Ok(LspInvocation::Definition {
                    path: arguments.path,
                    line: arguments.line,
                    column: arguments.column,
                    server: arguments.server,
                })
            } else {
                Ok(LspInvocation::Hover {
                    path: arguments.path,
                    line: arguments.line,
                    column: arguments.column,
                    server: arguments.server,
                })
            }
        }
        LSP_REFERENCES_TOOL => {
            let arguments: ReferencesArguments = parse_arguments(arguments)?;
            validate_position(&arguments.path, arguments.line, arguments.column)?;
            Ok(LspInvocation::References {
                path: arguments.path,
                line: arguments.line,
                column: arguments.column,
                include_declaration: arguments.include_declaration,
                server: arguments.server,
            })
        }
        _ => Err(LspError::InvalidInput(format!(
            "unknown built-in LSP function {function:?}"
        ))),
    }
}

fn parse_arguments<T: for<'de> Deserialize<'de>>(arguments: &str) -> Result<T, LspError> {
    if arguments.len() > 64 * 1024 {
        return Err(LspError::InvalidInput(
            "native function arguments exceed 65536 bytes".to_owned(),
        ));
    }
    serde_json::from_str(arguments)
        .map_err(|error| LspError::InvalidInput(format!("invalid JSON arguments: {error}")))
}

fn validate_path_argument(path: &str) -> Result<(), LspError> {
    if path.is_empty() || path.len() > crate::tools::MAX_MODEL_PATH_BYTES || path.contains('\0') {
        return Err(LspError::InvalidInput("invalid document path".to_owned()));
    }
    Ok(())
}

fn validate_position(path: &str, line: u64, column: u64) -> Result<(), LspError> {
    validate_path_argument(path)?;
    if line == 0 || column == 0 || line > 10_000_000 || column > 10_000_000 {
        return Err(LspError::InvalidInput(
            "line and column are one-based and must be between 1 and 10000000".to_owned(),
        ));
    }
    Ok(())
}

#[must_use]
pub fn is_lsp_function(name: &str) -> bool {
    matches!(
        name,
        LSP_STATUS_TOOL
            | LSP_DIAGNOSTICS_TOOL
            | LSP_DOCUMENT_SYMBOLS_TOOL
            | LSP_WORKSPACE_SYMBOLS_TOOL
            | LSP_DEFINITION_TOOL
            | LSP_REFERENCES_TOOL
            | LSP_HOVER_TOOL
    )
}

fn nullable_string() -> Value {
    json!({ "type": ["string", "null"], "maxLength": 16384 })
}

fn position_schema(include_declaration: bool) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "path".to_owned(),
            json!({ "type": "string", "minLength": 1, "maxLength": 16384 }),
        ),
        (
            "line".to_owned(),
            json!({ "type": "integer", "minimum": 1, "maximum": 10000000 }),
        ),
        (
            "column".to_owned(),
            json!({ "type": "integer", "minimum": 1, "maximum": 10000000 }),
        ),
        ("server".to_owned(), nullable_string()),
    ]);
    let mut required = vec!["path", "line", "column", "server"];
    if include_declaration {
        properties.insert(
            "include_declaration".to_owned(),
            json!({ "type": "boolean" }),
        );
        required.push("include_declaration");
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

#[must_use]
pub fn lsp_function_definitions() -> Vec<FunctionToolDefinition> {
    vec![
        FunctionToolDefinition::new(
            LSP_STATUS_TOOL,
            Some("List configured language servers, detection, connection state, and diagnostic counts. Read-only and local.".to_owned()),
            json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false }),
        ),
        FunctionToolDefinition::new(
            LSP_DIAGNOSTICS_TOOL,
            Some("Read bounded compiler/language-server diagnostics. path and severity are nullable filters; lines and columns in results are one-based.".to_owned()),
            json!({
                "type": "object",
                "properties": {
                    "path": nullable_string(),
                    "severity": { "type": ["string", "null"], "enum": ["error", "warning", "information", "hint", "unknown", null] }
                },
                "required": ["path", "severity"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            LSP_DOCUMENT_SYMBOLS_TOOL,
            Some("Return a compact symbol outline for one workspace file without reading the whole file. server may be null for automatic selection.".to_owned()),
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1, "maxLength": 16384 }, "server": nullable_string() },
                "required": ["path", "server"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            LSP_WORKSPACE_SYMBOLS_TOOL,
            Some("Search project symbols semantically. Use a configured server name when more than one language server is active.".to_owned()),
            json!({
                "type": "object",
                "properties": { "query": { "type": "string", "minLength": 1, "maxLength": 4096 }, "server": nullable_string() },
                "required": ["query", "server"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            LSP_DEFINITION_TOOL,
            Some("Find semantic definitions at a one-based workspace file line and column. Read-only.".to_owned()),
            position_schema(false),
        ),
        FunctionToolDefinition::new(
            LSP_REFERENCES_TOOL,
            Some("Find semantic references at a one-based workspace file line and column. Read-only and bounded.".to_owned()),
            position_schema(true),
        ),
        FunctionToolDefinition::new(
            LSP_HOVER_TOOL,
            Some("Ask the language server for inferred type/signature documentation at a one-based line and column.".to_owned()),
            position_schema(false),
        ),
    ]
}

fn normalize_diagnostic(server: &str, path: &str, value: &Value) -> Option<LspDiagnostic> {
    let range = value.get("range")?;
    let start = range.get("start")?;
    let end = range.get("end")?;
    let message = value.get("message")?.as_str()?;
    Some(LspDiagnostic {
        server: server.to_owned(),
        path: path.to_owned(),
        line: start.get("line").and_then(Value::as_u64)?.saturating_add(1),
        column: start
            .get("character")
            .and_then(Value::as_u64)?
            .saturating_add(1),
        end_line: end.get("line").and_then(Value::as_u64)?.saturating_add(1),
        end_column: end
            .get("character")
            .and_then(Value::as_u64)?
            .saturating_add(1),
        severity: LspDiagnosticSeverity::from_lsp(value.get("severity").and_then(Value::as_u64)),
        message: sanitize_text(message, MAX_TEXT_FIELD_BYTES),
        source: value
            .get("source")
            .and_then(Value::as_str)
            .map(|value| sanitize_text(value, 512)),
        code: value.get("code").and_then(|code| match code {
            Value::String(value) => Some(sanitize_text(value, 512)),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        }),
    })
}

fn diagnostics_to_json(diagnostics: &[LspDiagnostic]) -> Vec<Value> {
    diagnostics
        .iter()
        .map(|diagnostic| {
            json!({
                "server": diagnostic.server,
                "path": diagnostic.path,
                "line": diagnostic.line,
                "column": diagnostic.column,
                "end_line": diagnostic.end_line,
                "end_column": diagnostic.end_column,
                "severity": diagnostic.severity.label(),
                "message": diagnostic.message,
                "source": diagnostic.source,
                "code": diagnostic.code,
            })
        })
        .collect()
}

fn normalize_query_result(
    root: &Path,
    privacy: Option<&PrivacyShield>,
    server: &str,
    query: &LspQuery,
    value: &Value,
) -> Value {
    match query.operation.as_str() {
        "document_symbols" => Value::Array(normalize_symbols(root, privacy, value, true)),
        "workspace_symbols" => Value::Array(normalize_symbols(root, privacy, value, false)),
        "definition" | "references" => Value::Array(normalize_locations(root, privacy, value)),
        "hover" => normalize_hover(value),
        _ => json!({ "server": server, "available": value != &Value::Null }),
    }
}

fn normalize_symbols(
    root: &Path,
    privacy: Option<&PrivacyShield>,
    value: &Value,
    document_symbols: bool,
) -> Vec<Value> {
    let mut output = Vec::new();
    let Some(items) = value.as_array() else {
        return output;
    };
    if document_symbols {
        for item in items {
            collect_document_symbol(item, None, &mut output);
            if output.len() >= MAX_NORMALIZED_ITEMS {
                break;
            }
        }
    } else {
        for item in items {
            let location = item.get("location");
            let Some(path) = location
                .and_then(|location| location.get("uri"))
                .and_then(Value::as_str)
                .and_then(|uri| relative_uri_path(root, uri))
            else {
                continue;
            };
            if privacy.is_some_and(|shield| shield.check_relative(Path::new(&path), false).is_err())
            {
                continue;
            }
            output.push(json!({
                "name": sanitize_json_string(item.get("name")),
                "kind": item.get("kind").and_then(Value::as_u64),
                "container": sanitize_json_string(item.get("containerName")),
                "path": path,
                "range": normalize_range(location.and_then(|location| location.get("range"))),
            }));
            if output.len() >= MAX_NORMALIZED_ITEMS {
                break;
            }
        }
    }
    output
}

fn collect_document_symbol(value: &Value, parent: Option<&str>, output: &mut Vec<Value>) {
    if output.len() >= MAX_NORMALIZED_ITEMS {
        return;
    }
    let name = sanitize_json_string(value.get("name"));
    output.push(json!({
        "name": name,
        "kind": value.get("kind").and_then(Value::as_u64),
        "parent": parent,
        "range": normalize_range(value.get("selectionRange").or_else(|| value.get("range"))),
    }));
    let parent_name = name.as_deref().or(parent);
    if let Some(children) = value.get("children").and_then(Value::as_array) {
        for child in children {
            collect_document_symbol(child, parent_name, output);
            if output.len() >= MAX_NORMALIZED_ITEMS {
                break;
            }
        }
    }
}

fn normalize_locations(root: &Path, privacy: Option<&PrivacyShield>, value: &Value) -> Vec<Value> {
    let items = match value {
        Value::Array(items) => items.iter().collect::<Vec<_>>(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    };
    items
        .into_iter()
        .filter_map(|item| {
            let uri = item
                .get("uri")
                .or_else(|| item.get("targetUri"))
                .and_then(Value::as_str)?;
            let path = relative_uri_path(root, uri)?;
            if privacy.is_some_and(|shield| shield.check_relative(Path::new(&path), false).is_err())
            {
                return None;
            }
            let range = item
                .get("range")
                .or_else(|| item.get("targetSelectionRange"))
                .or_else(|| item.get("targetRange"));
            Some(json!({ "path": path, "range": normalize_range(range) }))
        })
        .take(MAX_NORMALIZED_ITEMS)
        .collect()
}

fn normalize_hover(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let contents = value.get("contents").map(hover_text).unwrap_or_default();
    json!({
        "contents": sanitize_text(&contents, MAX_TEXT_FIELD_BYTES),
        "range": normalize_range(value.get("range")),
    })
}

fn hover_text(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(values) => values.iter().map(hover_text).collect::<Vec<_>>().join("\n"),
        Value::Object(object) => object
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn normalize_range(value: Option<&Value>) -> Value {
    let Some(start) = value.and_then(|range| range.get("start")) else {
        return Value::Null;
    };
    let Some(end) = value.and_then(|range| range.get("end")) else {
        return Value::Null;
    };
    json!({
        "start": {
            "line": start.get("line").and_then(Value::as_u64).map(|value| value.saturating_add(1)),
            "column": start.get("character").and_then(Value::as_u64).map(|value| value.saturating_add(1)),
        },
        "end": {
            "line": end.get("line").and_then(Value::as_u64).map(|value| value.saturating_add(1)),
            "column": end.get("character").and_then(Value::as_u64).map(|value| value.saturating_add(1)),
        }
    })
}

fn sanitize_json_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|value| sanitize_text(value, MAX_TEXT_FIELD_BYTES))
}

fn relative_uri_path(root: &Path, uri: &str) -> Option<String> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    let scoped_path = match dunce::canonicalize(&path) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => path,
        Err(_) => return None,
    };
    let relative = match scoped_path.strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        Err(_) => {
            let canonical_root = dunce::canonicalize(root).ok()?;
            scoped_path.strip_prefix(canonical_root).ok()?.to_path_buf()
        }
    };
    Some(relative.to_string_lossy().replace('\\', "/"))
}

fn sanitize_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars() {
        if output.len() >= max_bytes {
            break;
        }
        let replacement = if character == '\n' || character == '\t' {
            character
        } else if character.is_control()
            || matches!(
                character,
                '\u{202A}'
                    ..='\u{202E}' | '\u{2066}'
                    ..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
            )
        {
            '\u{FFFD}'
        } else {
            character
        };
        if output.len().saturating_add(replacement.len_utf8()) > max_bytes {
            break;
        }
        output.push(replacement);
    }
    output
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::json;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use super::{
        LspConfig, LspConnectionState, LspError, LspManager, LspServerConfig, is_lsp_function,
        lsp_function_definitions, relative_uri_path, sanitize_text,
    };
    use crate::notice::UiNotice;

    fn server() -> LspServerConfig {
        LspServerConfig {
            name: "rust".to_owned(),
            enabled: true,
            required: false,
            auto_start: false,
            command: "rust-analyzer".to_owned(),
            args: Vec::new(),
            language_id: "rust".to_owned(),
            extensions: vec![".rs".to_owned()],
            root_markers: vec!["Cargo.toml".to_owned()],
        }
    }

    fn fixture_server() -> LspServerConfig {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        #[cfg(windows)]
        let (command, args) = {
            let script = format!(
                "$b=[Text.Encoding]::UTF8.GetBytes('{body}');$o=[Console]::OpenStandardOutput();$h=[Text.Encoding]::ASCII.GetBytes(\"Content-Length: $($b.Length)`r`n`r`n\");$o.Write($h,0,$h.Length);$o.Write($b,0,$b.Length);$o.Flush();Start-Sleep -Seconds 5"
            );
            (
                "powershell.exe".to_owned(),
                vec![
                    "-NoProfile".to_owned(),
                    "-NonInteractive".to_owned(),
                    "-Command".to_owned(),
                    script,
                ],
            )
        };
        #[cfg(unix)]
        let (command, args) = {
            let script = format!(
                "printf 'Content-Length: {}\\r\\n\\r\\n{}'; sleep 5",
                body.len(),
                body
            );
            ("/bin/sh".to_owned(), vec!["-c".to_owned(), script])
        };
        LspServerConfig {
            name: "fixture".to_owned(),
            enabled: true,
            required: false,
            auto_start: false,
            command,
            args,
            language_id: "fixture".to_owned(),
            extensions: Vec::new(),
            root_markers: Vec::new(),
        }
    }

    #[test]
    fn configuration_is_bounded_and_rejects_unsafe_markers() {
        let mut config = LspConfig {
            servers: vec![server()],
            ..LspConfig::default()
        };
        assert!(config.validate().is_ok());
        config.servers[0].root_markers = vec!["../Cargo.toml".to_owned()];
        assert!(config.validate().is_err());
        config.servers[0].root_markers = vec!["Cargo.toml".to_owned()];
        config.max_message_bytes = 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn detection_is_explicit_and_never_installs_or_starts_by_itself()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        fs::write(root.path().join("Cargo.toml"), "[package]\nname='demo'\n")?;
        let manager = LspManager::new(
            LspConfig {
                startup_timeout: Duration::from_secs(1),
                servers: vec![server()],
                ..LspConfig::default()
            },
            root.path(),
        )?;
        let snapshot = manager
            .snapshots()
            .into_iter()
            .next()
            .ok_or("missing snapshot")?;
        assert!(snapshot.detected);
        assert_eq!(snapshot.state, LspConnectionState::Disconnected);
        Ok(())
    }

    #[tokio::test]
    async fn real_stdio_initialize_and_bounded_shutdown_reap_the_server()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let mut manager = LspManager::new(
            LspConfig {
                startup_timeout: Duration::from_secs(3),
                request_timeout: Duration::from_millis(100),
                servers: vec![fixture_server()],
                ..LspConfig::default()
            },
            root.path(),
        )?;
        manager.connect("fixture").await?;
        assert_eq!(manager.snapshots()[0].state, LspConnectionState::Connected);
        let query = manager
            .call(
                super::LSP_WORKSPACE_SYMBOLS_TOOL,
                r#"{"query":"main","server":"fixture"}"#,
                &CancellationToken::new(),
            )
            .await;
        assert!(matches!(query, Err(LspError::Timeout { .. })));
        tokio::time::timeout(Duration::from_secs(3), manager.shutdown()).await?;
        assert_eq!(
            manager.snapshots()[0].state,
            LspConnectionState::Disconnected
        );
        Ok(())
    }

    #[tokio::test]
    async fn optional_auto_start_failure_is_isolated_and_visible()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let mut missing = fixture_server();
        missing.name = "missing".to_owned();
        missing.command = "decode-definitely-missing-lsp-fixture".to_owned();
        missing.args.clear();
        missing.auto_start = true;
        let mut manager = LspManager::new(
            LspConfig {
                startup_timeout: Duration::from_millis(300),
                servers: vec![missing],
                ..LspConfig::default()
            },
            root.path(),
        )?;
        manager.start().await?;
        let snapshot = manager.snapshots().into_iter().next().ok_or("snapshot")?;
        assert_eq!(snapshot.state, LspConnectionState::Error);
        assert!(matches!(
            snapshot.notice,
            UiNotice::ExternalError { ref detail } if detail.contains("failed to start")
        ));
        Ok(())
    }

    #[tokio::test]
    async fn privacy_shield_blocks_lsp_document_text() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        fs::write(root.path().join(".env"), "TOKEN=never-send")?;
        fs::write(root.path().join(".env.example"), "TOKEN=example")?;
        let manager = LspManager::new(LspConfig::default(), root.path())?;
        assert!(matches!(
            manager.load_document(".env", "text").await,
            Err(LspError::UnsafePath { .. })
        ));
        let template = manager.load_document(".env.example", "text").await;
        assert!(
            template.is_ok(),
            "template should remain readable: {template:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn privacy_shield_checks_the_resolved_document_target()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let sensitive = root.path().join(".env");
        let alias = root.path().join("safe");
        fs::create_dir(&sensitive)?;
        fs::write(sensitive.join("secret.rs"), "TOKEN=never-send")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&sensitive, &alias)?;
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(&alias)
                .arg(&sensitive)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            assert!(status.success(), "could not create test junction");
        }
        let manager = LspManager::new(LspConfig::default(), root.path())?;

        assert!(matches!(
            manager.load_document("safe/secret.rs", "rust").await,
            Err(LspError::UnsafePath { .. })
        ));
        let alias_uri = url::Url::from_file_path(alias.join("secret.rs"))
            .map_err(|()| "alias URI conversion failed")?;
        manager.shared.publish_diagnostics(
            "fixture",
            &json!({
                "uri": alias_uri,
                "diagnostics": [{
                    "range": {
                        "start": { "line": 0, "character": 0 },
                        "end": { "line": 0, "character": 1 }
                    },
                    "message": "TOKEN=never-send"
                }]
            }),
        );
        assert!(manager.diagnostics().is_empty());
        Ok(())
    }

    #[test]
    fn diagnostics_are_workspace_scoped_bounded_and_sanitized()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let source = root.path().join("src.rs");
        fs::write(&source, "fn main() {}")?;
        let manager = LspManager::new(
            LspConfig {
                max_diagnostics: 1,
                servers: vec![fixture_server()],
                ..LspConfig::default()
            },
            root.path(),
        )?;
        let source_uri = url::Url::from_file_path(&source).map_err(|()| "source URI")?;
        manager.shared.publish_diagnostics(
            "fixture",
            &json!({
                "uri": source_uri.as_str(),
                "diagnostics": [
                    {
                        "range": {
                            "start": { "line": 0, "character": 1 },
                            "end": { "line": 0, "character": 3 }
                        },
                        "severity": 1,
                        "message": "bad\u{1b}[31m\u{202e}",
                        "source": "fixture"
                    },
                    {
                        "range": {
                            "start": { "line": 1, "character": 0 },
                            "end": { "line": 1, "character": 1 }
                        },
                        "message": "must be truncated"
                    }
                ]
            }),
        );
        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].path, "src.rs");
        assert!(!diagnostics[0].message.contains('\u{1b}'));
        assert!(!diagnostics[0].message.contains('\u{202e}'));

        let outside = tempdir()?;
        let outside_file = outside.path().join("outside.rs");
        fs::write(&outside_file, "")?;
        let outside_uri = url::Url::from_file_path(&outside_file).map_err(|()| "outside URI")?;
        manager.shared.publish_diagnostics(
            "fixture",
            &json!({ "uri": outside_uri.as_str(), "diagnostics": [] }),
        );
        assert_eq!(manager.diagnostics().len(), 1);
        Ok(())
    }

    #[test]
    fn malformed_diagnostics_do_not_hide_later_valid_items()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let source = root.path().join("src.rs");
        fs::write(&source, "fn main() {}")?;
        let manager = LspManager::new(
            LspConfig {
                max_diagnostics: 1,
                servers: vec![fixture_server()],
                ..LspConfig::default()
            },
            root.path(),
        )?;
        let source_uri = url::Url::from_file_path(&source).map_err(|()| "source URI")?;
        manager.shared.publish_diagnostics(
            "fixture",
            &json!({
                "uri": source_uri.as_str(),
                "diagnostics": [
                    { "message": "missing range" },
                    {
                        "range": {
                            "start": { "line": 0, "character": 0 },
                            "end": { "line": 0, "character": 1 }
                        },
                        "message": "valid"
                    }
                ]
            }),
        );

        let diagnostics = manager.diagnostics();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].message, "valid");
        Ok(())
    }

    #[test]
    fn workspace_symbols_are_scoped_before_the_result_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let safe = root.path().join("src.rs");
        let sensitive = root.path().join(".env");
        let outside = tempdir()?.path().join("outside.rs");
        fs::write(&safe, "fn safe() {}")?;
        fs::write(&sensitive, "TOKEN=secret")?;
        let safe_uri = url::Url::from_file_path(&safe).map_err(|()| "safe URI")?;
        let sensitive_uri = url::Url::from_file_path(&sensitive).map_err(|()| "sensitive URI")?;
        let outside_uri = url::Url::from_file_path(&outside).map_err(|()| "outside URI")?;
        let privacy = crate::privacy::PrivacyShield::load_project_only(root.path())?;

        let outside_result = super::normalize_symbols(
            root.path(),
            Some(&privacy),
            &json!([{ "name": "outside", "location": { "uri": outside_uri } }]),
            false,
        );
        assert!(outside_result.is_empty());

        let mut symbols = (0..super::MAX_NORMALIZED_ITEMS)
            .map(|index| {
                json!({
                    "name": format!("secret-{index}"),
                    "location": { "uri": sensitive_uri }
                })
            })
            .collect::<Vec<_>>();
        symbols.push(json!({ "name": "safe", "location": { "uri": safe_uri } }));
        let result = super::normalize_symbols(
            root.path(),
            Some(&privacy),
            &serde_json::Value::Array(symbols),
            false,
        );
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["name"], "safe");
        Ok(())
    }

    #[test]
    fn locations_are_scoped_before_the_result_limit() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let safe = root.path().join("src.rs");
        let outside = tempdir()?.path().join("outside.rs");
        fs::write(&safe, "fn safe() {}")?;
        let safe_uri = url::Url::from_file_path(&safe).map_err(|()| "safe URI")?;
        let outside_uri = url::Url::from_file_path(&outside).map_err(|()| "outside URI")?;
        let mut locations = (0..super::MAX_NORMALIZED_ITEMS)
            .map(|_| json!({ "uri": outside_uri }))
            .collect::<Vec<_>>();
        locations.push(json!({ "uri": safe_uri }));

        let result =
            super::normalize_locations(root.path(), None, &serde_json::Value::Array(locations));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["path"], "src.rs");
        Ok(())
    }

    #[test]
    fn truncated_output_still_respects_the_configured_byte_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let manager = LspManager::new(
            LspConfig {
                max_result_bytes: 4 * 1024,
                ..LspConfig::default()
            },
            root.path(),
        )?;

        let output = manager.output(json!({ "value": "\"".repeat(10_000) }))?;
        assert!(output.truncated);
        assert!(output.content.len() <= manager.config.max_result_bytes);
        Ok(())
    }

    #[test]
    fn native_tools_are_read_only_strict_and_complete() -> Result<(), serde_json::Error> {
        let definitions = lsp_function_definitions();
        assert_eq!(definitions.len(), 7);
        assert!(
            definitions
                .iter()
                .all(|definition| is_lsp_function(&definition.name))
        );
        let value = serde_json::to_value(&definitions)?;
        assert!(
            value
                .as_array()
                .is_some_and(|items| items.iter().all(|item| {
                    item["strict"] == true && item["parameters"]["additionalProperties"] == false
                }))
        );
        Ok(())
    }

    #[test]
    fn uri_and_display_text_cannot_escape_or_inject_controls()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let file = root.path().join("src.rs");
        fs::write(&file, "fn main() {}")?;
        let uri = url::Url::from_file_path(&file).map_err(|()| "URI conversion failed")?;
        assert_eq!(
            relative_uri_path(root.path(), uri.as_str()).as_deref(),
            Some("src.rs")
        );
        assert_eq!(sanitize_text("bad\u{1b}[31m\u{202e}", 128), "bad�[31m�");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn relative_uri_path_accepts_a_workspace_root_alias() -> Result<(), Box<dyn std::error::Error>>
    {
        let real_root = tempdir()?;
        let aliases = tempdir()?;
        let root_alias = aliases.path().join("workspace-link");
        std::os::unix::fs::symlink(real_root.path(), &root_alias)?;
        let file = root_alias.join("src.rs");
        fs::write(real_root.path().join("src.rs"), "fn main() {}")?;
        let uri = url::Url::from_file_path(&file).map_err(|()| "URI conversion failed")?;

        assert_eq!(
            relative_uri_path(&root_alias, uri.as_str()).as_deref(),
            Some("src.rs")
        );
        Ok(())
    }
}
