use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use serde_json::Value;

use crate::notice::UiNotice;
use sha2::{Digest, Sha256};

use crate::{api::FunctionToolDefinition, error::ConfigError};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const DEFAULT_MAX_RESULT_BYTES: usize = 256 * 1024;
const DEFAULT_MAX_SSE_EVENT_BYTES: usize = 1024 * 1024;
const MAX_MCP_SERVERS: usize = 32;
pub(crate) const MAX_MCP_TOOLS: usize = 512;

#[derive(Debug, Clone)]
pub struct McpConfig {
    pub enabled: bool,
    pub startup_timeout: Duration,
    pub tool_timeout: Duration,
    pub max_result_bytes: usize,
    pub max_sse_event_bytes: usize,
    /// Maximum reconnects performed by the HTTP transport after the initial
    /// connection. `0` disables transparent reconnect completely.
    pub reconnect_max_attempts: u32,
    pub reconnect_base_delay: Duration,
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            tool_timeout: DEFAULT_TOOL_TIMEOUT,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            max_sse_event_bytes: DEFAULT_MAX_SSE_EVENT_BYTES,
            reconnect_max_attempts: 2,
            reconnect_base_delay: Duration::from_secs(1),
            servers: Vec::new(),
        }
    }
}

impl McpConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.startup_timeout.is_zero() || self.startup_timeout > Duration::from_secs(300) {
            return Err(invalid(
                "mcp.startup_timeout_secs",
                "must be between 1 and 300 seconds",
            ));
        }
        if self.tool_timeout.is_zero() || self.tool_timeout > Duration::from_secs(3600) {
            return Err(invalid(
                "mcp.tool_timeout_secs",
                "must be between 1 and 3600 seconds",
            ));
        }
        if !(1024..=8 * 1024 * 1024).contains(&self.max_result_bytes) {
            return Err(invalid(
                "mcp.max_result_bytes",
                "must be between 1024 and 8388608 bytes",
            ));
        }
        if !(1024..=16 * 1024 * 1024).contains(&self.max_sse_event_bytes) {
            return Err(invalid(
                "mcp.max_sse_event_bytes",
                "must be between 1024 and 16777216 bytes",
            ));
        }
        if self.reconnect_max_attempts > 5 {
            return Err(invalid(
                "mcp.reconnect_max_attempts",
                "must be between 0 and 5",
            ));
        }
        if self.reconnect_base_delay.is_zero()
            || self.reconnect_base_delay > Duration::from_secs(30)
        {
            return Err(invalid(
                "mcp.reconnect_base_delay_ms",
                "must be between 1 and 30000 milliseconds",
            ));
        }
        if self.servers.len() > MAX_MCP_SERVERS {
            return Err(invalid("mcp.servers", "must contain at most 32 servers"));
        }
        let mut names = BTreeSet::new();
        for server in &self.servers {
            server.validate()?;
            if !names.insert(server.name.to_ascii_lowercase()) {
                return Err(invalid(
                    "mcp.servers.name",
                    format!("duplicate server name {:?}", server.name),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub required: bool,
    pub transport: McpTransportConfig,
    pub permissions: McpPermissionConfig,
}

impl McpServerConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.name.trim().is_empty()
            || self.name.len() > 64
            || self.name.contains(char::is_control)
        {
            return Err(invalid(
                "mcp.servers.name",
                "must be a non-empty printable name no longer than 64 bytes",
            ));
        }
        self.transport.validate()?;
        self.permissions.validate()
    }
}

#[derive(Debug, Clone)]
pub enum McpTransportConfig {
    Stdio {
        command: String,
        args: Vec<String>,
        env_from: BTreeMap<String, String>,
        working_directory: Option<PathBuf>,
    },
    StreamableHttp {
        url: String,
        bearer_token_env: Option<String>,
        headers_from: BTreeMap<String, String>,
        oauth: Option<McpOAuthConfig>,
    },
}

impl McpTransportConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Stdio {
                command,
                args,
                env_from,
                working_directory,
            } => {
                if command.trim().is_empty() || command.contains('\0') {
                    return Err(invalid(
                        "mcp.servers.command",
                        "must be a non-empty executable name without NUL bytes",
                    ));
                }
                if args.len() > 128 || args.iter().any(|arg| arg.contains('\0')) {
                    return Err(invalid(
                        "mcp.servers.args",
                        "must contain at most 128 NUL-free arguments",
                    ));
                }
                validate_env_map("mcp.servers.env_from", env_from)?;
                if working_directory
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
                {
                    return Err(invalid(
                        "mcp.servers.working_directory",
                        "must be absolute when set",
                    ));
                }
            }
            Self::StreamableHttp {
                url,
                bearer_token_env,
                headers_from,
                oauth,
            } => {
                validate_http_url(url)?;
                if bearer_token_env
                    .as_deref()
                    .is_some_and(|name| !valid_env_name(name))
                {
                    return Err(invalid(
                        "mcp.servers.bearer_token_env",
                        "must name an environment variable",
                    ));
                }
                validate_header_map("mcp.servers.headers_from", headers_from)?;
                if bearer_token_env.is_some() && oauth.is_some() {
                    return Err(invalid(
                        "mcp.servers",
                        "bearer_token_env and oauth cannot both be configured",
                    ));
                }
                if let Some(oauth) = oauth {
                    oauth.validate()?;
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "STDIO",
            Self::StreamableHttp { .. } => "HTTP",
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpOAuthConfig {
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub callback_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpOAuthPrompt {
    pub server: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    pub browser_opened: bool,
}

impl McpOAuthConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.client_id.as_deref().is_some_and(str::is_empty) {
            return Err(invalid(
                "mcp.servers.oauth.client_id",
                "must not be empty when set",
            ));
        }
        if self.scopes.len() > 64
            || self
                .scopes
                .iter()
                .any(|scope| scope.is_empty() || scope.contains(char::is_whitespace))
        {
            return Err(invalid(
                "mcp.servers.oauth.scopes",
                "must contain at most 64 non-empty whitespace-free scopes",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpApprovalMode {
    Always,
    Writes,
    Never,
}

#[derive(Debug, Clone)]
pub struct McpPermissionConfig {
    pub approval: McpApprovalMode,
    pub enabled_tools: BTreeSet<String>,
    pub disabled_tools: BTreeSet<String>,
    /// Tools listed here may skip approval in `Writes` mode only if the MCP
    /// server also advertises `readOnlyHint=true`. Both signals are required.
    pub trusted_read_only_tools: BTreeSet<String>,
}

impl Default for McpPermissionConfig {
    fn default() -> Self {
        Self {
            approval: McpApprovalMode::Always,
            enabled_tools: BTreeSet::new(),
            disabled_tools: BTreeSet::new(),
            trusted_read_only_tools: BTreeSet::new(),
        }
    }
}

impl McpPermissionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self
            .enabled_tools
            .iter()
            .any(|tool| self.disabled_tools.contains(tool))
        {
            return Err(invalid(
                "mcp.servers.permissions",
                "the same tool cannot be both enabled and disabled",
            ));
        }
        for tool in self
            .enabled_tools
            .iter()
            .chain(&self.disabled_tools)
            .chain(&self.trusted_read_only_tools)
        {
            if tool.is_empty() || tool.len() > 128 || tool.contains(char::is_control) {
                return Err(invalid(
                    "mcp.servers.permissions",
                    "tool names must be printable and no longer than 128 bytes",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpConnectionState {
    Disabled,
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
    ReauthRequired,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSnapshot {
    pub name: String,
    pub transport: &'static str,
    /// Whether the global MCP runtime is allowed by trusted configuration.
    pub runtime_available: bool,
    /// Per-server runtime switch. Its startup value comes from config.toml.
    pub enabled: bool,
    pub required: bool,
    pub oauth: bool,
    pub state: McpConnectionState,
    pub tool_count: usize,
    pub notice: UiNotice,
}

#[derive(Debug, Clone)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub function_name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub input_schema: Value,
    pub read_only_hint: Option<bool>,
    pub destructive_hint: Option<bool>,
    pub open_world_hint: Option<bool>,
}

impl McpTool {
    #[must_use]
    pub fn function_definition(&self) -> FunctionToolDefinition {
        let description = self.description.as_deref().map_or_else(
            || format!("MCP tool {} from server {}", self.name, self.server),
            |description| format!("[{server}] {description}", server = self.server),
        );
        FunctionToolDefinition::new(
            self.function_name.clone(),
            Some(limit_string(description, 1024)),
            self.input_schema.clone(),
        )
        // MCP accepts general JSON Schema, while Responses strict functions
        // require a narrower shape (for example, closed object properties).
        // Advertising an arbitrary server schema as strict can make Azure
        // reject the whole request before the tool is ever called.
        .with_strict(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutput {
    pub content: String,
    pub is_error: bool,
    pub truncated: bool,
}

pub(crate) fn assign_function_names(tools: &mut [McpTool]) {
    let mut used = BTreeSet::new();
    for tool in tools {
        let full_identity = format!("{}\0{}", tool.server, tool.name);
        let mut candidate = format!(
            "mcp__{}__{}",
            normalized_component(&tool.server),
            normalized_component(&tool.name)
        );
        if candidate.len() > 64 || !used.insert(candidate.clone()) {
            let digest = Sha256::digest(full_identity.as_bytes());
            let hash = digest[..5]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let mut discriminator = 0_u32;
            loop {
                let suffix = if discriminator == 0 {
                    hash.clone()
                } else {
                    format!("{hash}_{discriminator}")
                };
                let prefix_len = 64_usize.saturating_sub(suffix.len()).saturating_sub(2);
                candidate.truncate(prefix_len.min(candidate.len()));
                candidate.push_str("__");
                candidate.push_str(&suffix);
                if used.insert(candidate.clone()) {
                    break;
                }
                candidate = format!(
                    "mcp__{}__{}",
                    normalized_component(&tool.server),
                    normalized_component(&tool.name)
                );
                discriminator = discriminator.saturating_add(1);
            }
        }
        tool.function_name = candidate;
    }
}

fn normalized_component(value: &str) -> String {
    let mut result = String::new();
    let mut previous_underscore = false;
    for character in value.chars() {
        let mapped = if character.is_ascii_alphanumeric() {
            character.to_ascii_lowercase()
        } else {
            '_'
        };
        if mapped == '_' {
            if previous_underscore {
                continue;
            }
            previous_underscore = true;
        } else {
            previous_underscore = false;
        }
        result.push(mapped);
    }
    let trimmed = result.trim_matches('_');
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn limit_string(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    value
}

fn validate_env_map(
    field: &'static str,
    values: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    if values.len() > 64
        || values
            .iter()
            .any(|(target, source)| !valid_env_name(target) || !valid_env_name(source))
    {
        return Err(invalid(
            field,
            "must contain at most 64 TARGET=SOURCE environment variable names",
        ));
    }
    Ok(())
}

fn validate_header_map(
    field: &'static str,
    values: &BTreeMap<String, String>,
) -> Result<(), ConfigError> {
    if values.len() > 64
        || values.iter().any(|(header, source)| {
            header.is_empty()
                || header.len() > 128
                || !header.bytes().all(is_http_token_byte)
                || !valid_env_name(source)
        })
    {
        return Err(invalid(
            field,
            "must contain at most 64 HTTP_HEADER=SOURCE_ENV mappings",
        ));
    }
    Ok(())
}

const fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn valid_env_name(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn validate_http_url(value: &str) -> Result<(), ConfigError> {
    let url = reqwest::Url::parse(value)
        .map_err(|error| invalid("mcp.servers.url", error.to_string()))?;
    if url.host_str().is_none()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid(
            "mcp.servers.url",
            "must be an absolute URL without credentials or fragment",
        ));
    }
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(invalid(
            "mcp.servers.url",
            "must use HTTPS; HTTP is permitted only for loopback development servers",
        ));
    }
    Ok(())
}

fn invalid(field: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidValue {
        field,
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(server: &str, name: &str) -> McpTool {
        McpTool {
            server: server.to_owned(),
            name: name.to_owned(),
            function_name: String::new(),
            title: None,
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only_hint: None,
            destructive_hint: None,
            open_world_hint: None,
        }
    }

    #[test]
    fn function_names_are_bounded_valid_and_collision_resistant() {
        let mut tools = vec![
            tool("Git Hub", "read.issue"),
            tool("git-hub", "read issue"),
            tool(&"very long server ".repeat(8), &"very long tool ".repeat(8)),
        ];
        assign_function_names(&mut tools);
        let names = tools
            .iter()
            .map(|tool| tool.function_name.as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|name| name.len() <= 64));
        assert!(names.iter().all(|name| {
            name.chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        }));
    }

    #[test]
    fn generated_hash_name_cannot_collide_with_an_existing_plain_name() {
        let long = tool(&"z".repeat(100), "long_tool");
        let mut probe = vec![long.clone()];
        assign_function_names(&mut probe);
        let generated = probe[0].function_name.clone();
        let remainder = generated.strip_prefix("mcp__").unwrap_or_default();
        let (server, name) = remainder.rsplit_once("__").unwrap_or(("plain", remainder));

        let mut tools = vec![tool(server, name), long];
        assign_function_names(&mut tools);
        assert_ne!(tools[0].function_name, tools[1].function_name);
    }

    #[test]
    fn http_validation_rejects_credentials_fragments_and_remote_plaintext() {
        assert!(validate_http_url("https://mcp.example.test/mcp").is_ok());
        assert!(validate_http_url("http://127.0.0.1:8080/mcp").is_ok());
        assert!(validate_http_url("http://mcp.example.test/mcp").is_err());
        assert!(validate_http_url("https://user:pass@example.test/mcp").is_err());
        assert!(validate_http_url("https://example.test/mcp#secret").is_err());
    }

    #[test]
    fn external_mcp_schema_is_not_misrepresented_as_responses_strict()
    -> Result<(), serde_json::Error> {
        let definition = tool("files", "read").function_definition();
        let value = serde_json::to_value(definition)?;
        assert_eq!(value["strict"], false);
        Ok(())
    }
}
