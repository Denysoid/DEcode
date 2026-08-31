use std::{
    collections::{BTreeMap, HashMap},
    panic::AssertUnwindSafe,
    process::Stdio,
    sync::Arc,
};

use futures_util::FutureExt;
use http::{HeaderName, HeaderValue};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, Tool},
    service::RunningService,
    transport::{
        AuthClient, AuthError, AuthorizationManager, CredentialStore,
        StreamableHttpClientTransport, TokioChildProcess,
        common::client_side_sse::ExponentialBackoff,
        streamable_http_client::StreamableHttpClientTransportConfig, which_command,
    },
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use tokio::{
    io::AsyncReadExt,
    task::JoinHandle,
    time::{Instant, timeout},
};

use super::{
    McpCallOutput, McpConfig, McpConnectionState, McpError, McpOAuthPrompt, McpPermissionDecision,
    McpServerConfig, McpServerSnapshot, McpTool, McpTransportConfig, evaluate_permission,
    oauth::{PendingOAuth, begin_authorization, manager_with_store, oauth_error},
    types::{MAX_MCP_TOOLS, assign_function_names},
};
use crate::{notice::UiNotice, redaction::redact_secret_values};

type ClientService = RunningService<RoleClient, ()>;
const MAX_AUTO_STARTUP_WINDOWS: u32 = 4;

struct RuntimeServer {
    config: McpServerConfig,
    state: McpConnectionState,
    notice: UiNotice,
    tools: Vec<McpTool>,
    service: Option<ClientService>,
    stderr_task: Option<JoinHandle<()>>,
    pending_oauth: Option<PendingOAuth>,
}

impl RuntimeServer {
    fn snapshot(&self) -> McpServerSnapshot {
        McpServerSnapshot {
            name: self.config.name.clone(),
            transport: self.config.transport.label(),
            runtime_available: true,
            enabled: self.config.enabled,
            required: self.config.required,
            oauth: matches!(
                &self.config.transport,
                McpTransportConfig::StreamableHttp { oauth: Some(_), .. }
            ),
            state: self.state,
            tool_count: self.tools.len(),
            notice: self.notice.clone(),
        }
    }
}

/// Owns all MCP connections. The manager is deliberately driven by the agent
/// actor rather than spawning an unbounded self-healing supervisor: every
/// startup, list, call, reconnect and shutdown operation has an explicit cap.
pub struct McpManager {
    config: McpConfig,
    servers: BTreeMap<String, RuntimeServer>,
    tools: Vec<McpTool>,
}

impl McpManager {
    pub fn new(config: McpConfig) -> Result<Self, crate::error::ConfigError> {
        config.validate()?;
        let servers = config
            .servers
            .iter()
            .cloned()
            .map(|server| {
                let state = if config.enabled && server.enabled {
                    McpConnectionState::Connecting
                } else {
                    McpConnectionState::Disabled
                };
                (
                    server.name.clone(),
                    RuntimeServer {
                        config: server,
                        state,
                        notice: UiNotice::None,
                        tools: Vec::new(),
                        service: None,
                        stderr_task: None,
                        pending_oauth: None,
                    },
                )
            })
            .collect();
        Ok(Self {
            config,
            servers,
            tools: Vec::new(),
        })
    }

    #[must_use]
    pub fn snapshots(&self) -> Vec<McpServerSnapshot> {
        self.servers
            .values()
            .map(|server| {
                let mut snapshot = server.snapshot();
                snapshot.runtime_available = self.config.enabled;
                snapshot
            })
            .collect()
    }

    #[must_use]
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Connect every enabled server. Optional servers degrade independently;
    /// a server marked `required=true` makes startup fail closed.
    pub async fn start(&mut self) -> Result<(), McpError> {
        if !self.config.enabled {
            return Ok(());
        }
        let names = self
            .servers
            .values()
            .filter(|server| server.config.enabled)
            .map(|server| server.config.name.clone())
            .collect::<Vec<_>>();
        let startup_budget = self
            .config
            .startup_timeout
            .saturating_mul(MAX_AUTO_STARTUP_WINDOWS);
        let startup_started = Instant::now();
        for name in names {
            let remaining = startup_budget.saturating_sub(startup_started.elapsed());
            if remaining.is_zero() {
                let required = self
                    .servers
                    .get(&name)
                    .is_some_and(|server| server.config.required);
                let error = McpError::StartupTimeout {
                    server: name.clone(),
                    secs: startup_budget.as_secs(),
                };
                self.set_state(
                    &name,
                    McpConnectionState::Disconnected,
                    UiNotice::external(error.to_string()),
                );
                tracing::warn!(server = %name, error = %error, "MCP server unavailable");
                if required {
                    return Err(error);
                }
                continue;
            }
            let result = timeout(remaining, self.connect(&name)).await;
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(_) => McpError::StartupTimeout {
                    server: name.clone(),
                    secs: startup_budget.as_secs(),
                },
            };
            let required = self
                .servers
                .get(&name)
                .is_some_and(|server| server.config.required);
            self.set_state(
                &name,
                McpConnectionState::Error,
                UiNotice::external(error.to_string()),
            );
            tracing::warn!(server = %name, error = %error, "MCP server unavailable");
            if required {
                return Err(error);
            }
        }
        Ok(())
    }

    pub async fn connect(&mut self, server_name: &str) -> Result<(), McpError> {
        if !self.config.enabled {
            return Err(McpError::RuntimeDisabled);
        }
        let server_config = self
            .servers
            .get(server_name)
            .map(|server| server.config.clone())
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        if !server_config.enabled {
            return Err(McpError::NotConnected {
                server: server_name.to_owned(),
                reason: "disabled by the runtime switch".to_owned(),
            });
        }
        self.disconnect(server_name).await;
        self.set_state(
            server_name,
            McpConnectionState::Connecting,
            UiNotice::LspStarting,
        );

        let connection = match &server_config.transport {
            McpTransportConfig::Stdio { .. } => self.connect_stdio(&server_config).await,
            McpTransportConfig::StreamableHttp { oauth: Some(_), .. } => {
                self.connect_oauth_from_store(&server_config).await
            }
            McpTransportConfig::StreamableHttp { .. } => {
                self.connect_streamable_http(&server_config).await
            }
        };
        let (service, stderr_task) = match connection {
            Ok(connection) => connection,
            Err(error) => {
                let error =
                    redact_mcp_error(&server_config, error, |name| std::env::var(name).ok());
                let state = if matches!(error, McpError::OAuthReauthRequired { .. }) {
                    McpConnectionState::ReauthRequired
                } else {
                    McpConnectionState::Error
                };
                self.set_state(server_name, state, UiNotice::external(error.to_string()));
                return Err(error);
            }
        };

        self.install_connection(server_name, service, stderr_task)
            .await
            .map_err(|error| {
                redact_mcp_error(&server_config, error, |name| std::env::var(name).ok())
            })
    }

    async fn install_connection(
        &mut self,
        server_name: &str,
        service: ClientService,
        stderr_task: Option<JoinHandle<()>>,
    ) -> Result<(), McpError> {
        let list_result = timeout(self.config.tool_timeout, service.peer().list_all_tools()).await;
        let raw_tools = match list_result {
            Ok(Ok(tools)) => tools,
            Ok(Err(error)) => {
                let error = McpError::Protocol {
                    server: server_name.to_owned(),
                    message: format!("tools/list failed: {error}"),
                };
                self.set_state(
                    server_name,
                    McpConnectionState::Error,
                    UiNotice::external(error.to_string()),
                );
                return Err(error);
            }
            Err(_) => {
                let error = McpError::OperationTimeout {
                    server: server_name.to_owned(),
                    operation: "tools/list".to_owned(),
                    secs: self.config.tool_timeout.as_secs(),
                };
                self.set_state(
                    server_name,
                    McpConnectionState::Error,
                    UiNotice::external(error.to_string()),
                );
                return Err(error);
            }
        };
        let tools = raw_tools
            .into_iter()
            .map(|tool| convert_tool(server_name, tool))
            .collect::<Vec<_>>();
        let other_tool_count = self
            .servers
            .iter()
            .filter(|(name, _)| name.as_str() != server_name)
            .map(|(_, server)| server.tools.len())
            .sum::<usize>();
        if tools.len().saturating_add(other_tool_count) > MAX_MCP_TOOLS {
            let error = McpError::Protocol {
                server: server_name.to_owned(),
                message: format!(
                    "tool registry would exceed the client limit of {MAX_MCP_TOOLS} tools"
                ),
            };
            self.set_state(
                server_name,
                McpConnectionState::Error,
                UiNotice::external(error.to_string()),
            );
            return Err(error);
        }
        if let Some(server) = self.servers.get_mut(server_name) {
            server.state = McpConnectionState::Connected;
            server.notice = UiNotice::McpToolsReady { count: tools.len() };
            server.tools = tools;
            server.service = Some(service);
            server.stderr_task = stderr_task;
        }
        self.rebuild_tool_index();
        Ok(())
    }

    pub async fn disconnect(&mut self, server_name: &str) {
        let Some(server) = self.servers.get_mut(server_name) else {
            return;
        };
        if let Some(mut service) = server.service.take() {
            let _ = service
                .close_with_timeout(self.config.startup_timeout)
                .await;
        }
        if let Some(task) = server.stderr_task.take() {
            task.abort();
        }
        server.pending_oauth = None;
        server.tools.clear();
        server.state = if self.config.enabled && server.config.enabled {
            McpConnectionState::Disconnected
        } else {
            McpConnectionState::Disabled
        };
        server.notice = UiNotice::Stopped;
        self.rebuild_tool_index();
    }

    /// Change one server's runtime availability without rewriting the trusted
    /// TOML file. Disabling is immediate and revokes its advertised tools;
    /// enabling prepares it for an explicit bounded connection attempt.
    pub async fn set_enabled(&mut self, server_name: &str, enabled: bool) -> Result<(), McpError> {
        if enabled && !self.config.enabled {
            return Err(McpError::RuntimeDisabled);
        }
        let server = self
            .servers
            .get_mut(server_name)
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        if server.config.enabled == enabled {
            return Ok(());
        }
        server.config.enabled = enabled;
        if !enabled {
            self.disconnect(server_name).await;
            if let Some(server) = self.servers.get_mut(server_name) {
                server.state = McpConnectionState::Disabled;
                server.notice = UiNotice::None;
            }
        } else if let Some(server) = self.servers.get_mut(server_name) {
            server.state = McpConnectionState::Disconnected;
            server.notice = UiNotice::None;
        }
        self.rebuild_tool_index();
        Ok(())
    }

    /// Adds one already validated user-managed server without restarting the
    /// TUI. The server is disconnected until the caller explicitly connects
    /// it, so saving a configuration never starts a process by surprise.
    pub fn add_server(&mut self, server: McpServerConfig) -> Result<(), crate::error::ConfigError> {
        self.validate_add(&server)?;
        let state = if self.config.enabled && server.enabled {
            McpConnectionState::Disconnected
        } else {
            McpConnectionState::Disabled
        };
        self.config.servers.push(server.clone());
        self.servers.insert(
            server.name.clone(),
            RuntimeServer {
                config: server,
                state,
                notice: UiNotice::None,
                tools: Vec::new(),
                service: None,
                stderr_task: None,
                pending_oauth: None,
            },
        );
        Ok(())
    }

    pub fn validate_add(&self, server: &McpServerConfig) -> Result<(), crate::error::ConfigError> {
        server.validate()?;
        if self.servers.contains_key(&server.name)
            || self
                .servers
                .keys()
                .any(|name| name.eq_ignore_ascii_case(&server.name))
        {
            return Err(crate::error::ConfigError::InvalidValue {
                field: "mcp.servers.name",
                message: format!("duplicate server name {:?}", server.name),
            });
        }
        if self.servers.len() >= 32 {
            return Err(crate::error::ConfigError::InvalidValue {
                field: "mcp.servers",
                message: "must contain at most 32 servers".to_owned(),
            });
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) {
        let names = self.servers.keys().cloned().collect::<Vec<_>>();
        for name in names {
            self.disconnect(&name).await;
        }
    }

    /// Begin an OAuth 2.0 Authorization Code + PKCE flow. The local callback
    /// listener is loopback-only, bounded, and validates both `code` and
    /// `state`; the browser is opened only after an explicit UI action.
    pub async fn begin_oauth(&mut self, server_name: &str) -> Result<McpOAuthPrompt, McpError> {
        if !self.config.enabled {
            return Err(McpError::RuntimeDisabled);
        }
        let server_config = self
            .servers
            .get(server_name)
            .map(|server| server.config.clone())
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        if !server_config.enabled {
            return Err(McpError::NotConnected {
                server: server_name.to_owned(),
                reason: "disabled by the runtime switch".to_owned(),
            });
        }
        let McpTransportConfig::StreamableHttp {
            url,
            oauth: Some(oauth),
            ..
        } = &server_config.transport
        else {
            return Err(McpError::OAuth {
                server: server_name.to_owned(),
                message: "this server is not configured for OAuth".to_owned(),
            });
        };
        self.disconnect(server_name).await;
        let (pending, authorization_url) = begin_authorization(server_name, url, oauth).await?;
        let redirect_uri = pending.redirect_uri.clone();
        let browser_opened = match open::that_detached(&authorization_url) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(server = %server_name, error = %error, "could not open OAuth browser");
                false
            }
        };
        if let Some(server) = self.servers.get_mut(server_name) {
            server.pending_oauth = Some(pending);
            server.state = McpConnectionState::ReauthRequired;
            server.notice = UiNotice::None;
        }
        Ok(McpOAuthPrompt {
            server: server_name.to_owned(),
            authorization_url,
            redirect_uri,
            browser_opened,
        })
    }

    /// Non-blocking callback poll for the TUI tick. Returns `true` exactly
    /// once, after the callback was exchanged and the MCP connection is ready.
    pub async fn poll_oauth(&mut self, server_name: &str) -> Result<bool, McpError> {
        let callback_result = {
            let server =
                self.servers
                    .get_mut(server_name)
                    .ok_or_else(|| McpError::UnknownServer {
                        server: server_name.to_owned(),
                    })?;
            let Some(pending) = server.pending_oauth.as_mut() else {
                return Ok(false);
            };
            match pending.callback_rx.try_recv() {
                Ok(callback) => Some(callback.map_err(|error| McpError::OAuthCallback {
                    server: server_name.to_owned(),
                    message: error.to_string(),
                })),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return Ok(false),
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    Some(Err(McpError::OAuthCallback {
                        server: server_name.to_owned(),
                        message: "callback listener closed before receiving authorization"
                            .to_owned(),
                    }))
                }
            }
        };
        let callback = match callback_result {
            Some(Ok(callback)) => callback,
            Some(Err(error)) => {
                if let Some(server) = self.servers.get_mut(server_name) {
                    server.pending_oauth = None;
                }
                self.set_state(
                    server_name,
                    McpConnectionState::Error,
                    UiNotice::external(error.to_string()),
                );
                return Err(error);
            }
            None => return Ok(false),
        };
        let pending = self
            .servers
            .get_mut(server_name)
            .and_then(|server| server.pending_oauth.take())
            .ok_or_else(|| McpError::OAuthCallback {
                server: server_name.to_owned(),
                message: "authorization session disappeared".to_owned(),
            })?;
        let exchange = match timeout(
            self.config.tool_timeout,
            pending.session.handle_callback_url(&callback),
        )
        .await
        {
            Ok(result) => result.map_err(|error| oauth_error(server_name, error)),
            Err(_) => Err(McpError::OperationTimeout {
                server: server_name.to_owned(),
                operation: "OAuth token exchange".to_owned(),
                secs: self.config.tool_timeout.as_secs(),
            }),
        };
        if let Err(error) = exchange {
            let state = if matches!(error, McpError::OAuthReauthRequired { .. }) {
                McpConnectionState::ReauthRequired
            } else {
                McpConnectionState::Error
            };
            self.set_state(server_name, state, UiNotice::external(error.to_string()));
            return Err(error);
        }
        let manager = pending.session.auth_manager;
        let server_config = self
            .servers
            .get(server_name)
            .map(|server| server.config.clone())
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        self.set_state(
            server_name,
            McpConnectionState::Connecting,
            UiNotice::LspStarting,
        );
        let connection = self.connect_authorized_http(&server_config, manager).await;
        let (service, stderr_task) = match connection {
            Ok(connection) => connection,
            Err(error) => {
                let state = if matches!(error, McpError::OAuthReauthRequired { .. }) {
                    McpConnectionState::ReauthRequired
                } else {
                    McpConnectionState::Error
                };
                self.set_state(server_name, state, UiNotice::external(error.to_string()));
                return Err(error);
            }
        };
        self.install_connection(server_name, service, stderr_task)
            .await?;
        Ok(true)
    }

    pub async fn forget_oauth(&mut self, server_name: &str) -> Result<(), McpError> {
        let server_config = self
            .servers
            .get(server_name)
            .map(|server| server.config.clone())
            .ok_or_else(|| McpError::UnknownServer {
                server: server_name.to_owned(),
            })?;
        let McpTransportConfig::StreamableHttp {
            url,
            oauth: Some(_),
            ..
        } = &server_config.transport
        else {
            return Err(McpError::OAuth {
                server: server_name.to_owned(),
                message: "this server is not configured for OAuth".to_owned(),
            });
        };
        self.disconnect(server_name).await;
        let (_manager, store) = manager_with_store(server_name, url).await?;
        store
            .clear()
            .await
            .map_err(|error| oauth_error(server_name, error))?;
        self.set_state(
            server_name,
            McpConnectionState::ReauthRequired,
            UiNotice::Stopped,
        );
        Ok(())
    }

    pub fn permission_for(&self, function_name: &str) -> Result<McpPermissionDecision, McpError> {
        let tool = self.tool_by_function(function_name)?;
        let server = self
            .servers
            .get(&tool.server)
            .ok_or_else(|| McpError::UnknownServer {
                server: tool.server.clone(),
            })?;
        Ok(evaluate_permission(&server.config.permissions, tool))
    }

    /// Return the immutable metadata bound to a native function name. Approval
    /// dialogs clone this value so a later registry refresh cannot change the
    /// server/tool identity the user reviewed.
    pub fn tool(&self, function_name: &str) -> Result<McpTool, McpError> {
        self.tool_by_function(function_name).cloned()
    }

    #[tracing::instrument(
        name = "mcp.call",
        level = "info",
        skip_all,
        fields(function = %function_name, status = "active")
    )]
    pub async fn call(
        &self,
        function_name: &str,
        arguments: Value,
        user_approved: bool,
    ) -> Result<McpCallOutput, McpError> {
        let tool = self.tool_by_function(function_name)?.clone();
        match self.permission_for(function_name)? {
            McpPermissionDecision::Deny { reason } => {
                return Err(McpError::PermissionDenied { reason });
            }
            McpPermissionDecision::RequireApproval { reason } if !user_approved => {
                return Err(McpError::PermissionDenied { reason });
            }
            McpPermissionDecision::Allow | McpPermissionDecision::RequireApproval { .. } => {}
        }
        let arguments = match arguments {
            Value::Object(arguments) => arguments,
            _ => return Err(McpError::InvalidArguments),
        };
        let server = self
            .servers
            .get(&tool.server)
            .ok_or_else(|| McpError::UnknownServer {
                server: tool.server.clone(),
            })?;
        let service = server
            .service
            .as_ref()
            .filter(|service| !service.is_closed())
            .ok_or_else(|| McpError::NotConnected {
                server: tool.server.clone(),
                reason: match &server.notice {
                    UiNotice::ExternalError { detail } | UiNotice::Legacy { detail } => {
                        detail.clone()
                    }
                    _ => format!("MCP server state: {:?}", server.state),
                },
            })?;
        let request = CallToolRequestParams::new(tool.name.clone()).with_arguments(arguments);
        let operation = async {
            timeout(self.config.tool_timeout, service.call_tool(request))
                .await
                .map_err(|_| McpError::OperationTimeout {
                    server: tool.server.clone(),
                    operation: format!("tools/call {}", tool.name),
                    secs: self.config.tool_timeout.as_secs(),
                })?
                .map_err(|error| McpError::Protocol {
                    server: tool.server.clone(),
                    message: format!("tools/call {} failed: {error}", tool.name),
                })
        };
        let result = match AssertUnwindSafe(operation).catch_unwind().await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => {
                return Err(redact_mcp_error(&server.config, error, |name| {
                    std::env::var(name).ok()
                }));
            }
            Err(_) => {
                return Err(McpError::DependencyPanic {
                    server: tool.server.clone(),
                    operation: format!("tools/call {}", tool.name),
                });
            }
        };
        bounded_call_output(result, self.config.max_result_bytes, &tool.server)
    }

    fn tool_by_function(&self, function_name: &str) -> Result<&McpTool, McpError> {
        self.tools
            .iter()
            .find(|tool| tool.function_name == function_name)
            .ok_or_else(|| McpError::UnknownTool {
                server: "<native-function-registry>".to_owned(),
                tool: function_name.to_owned(),
            })
    }

    async fn connect_stdio(
        &self,
        server: &McpServerConfig,
    ) -> Result<(ClientService, Option<JoinHandle<()>>), McpError> {
        let McpTransportConfig::Stdio {
            command,
            args,
            env_from,
            working_directory,
        } = &server.transport
        else {
            return Err(McpError::Startup {
                server: server.name.clone(),
                message: "internal transport mismatch".to_owned(),
            });
        };
        let mut process = which_command(command).map_err(|error| McpError::Startup {
            server: server.name.clone(),
            message: format!("cannot resolve executable {command:?}: {error}"),
        })?;
        process.args(args).kill_on_drop(true);
        if let Some(directory) = working_directory {
            process.current_dir(directory);
        }
        for (target, source) in env_from {
            let value = std::env::var(source).map_err(|_| McpError::Startup {
                server: server.name.clone(),
                message: format!("required environment variable {source:?} is not set"),
            })?;
            process.env(target, value);
        }
        let (transport, stderr) = TokioChildProcess::builder(process)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| McpError::Startup {
                server: server.name.clone(),
                message: error.to_string(),
            })?;
        let stderr_task = stderr.map(|stderr| drain_stderr(server.name.clone(), stderr));
        let service = timeout(self.config.startup_timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::StartupTimeout {
                server: server.name.clone(),
                secs: self.config.startup_timeout.as_secs(),
            })?
            .map_err(|error| McpError::Startup {
                server: server.name.clone(),
                message: error.to_string(),
            })?;
        Ok((service, stderr_task))
    }

    async fn connect_streamable_http(
        &self,
        server: &McpServerConfig,
    ) -> Result<(ClientService, Option<JoinHandle<()>>), McpError> {
        let McpTransportConfig::StreamableHttp {
            url,
            bearer_token_env,
            headers_from,
            oauth: None,
        } = &server.transport
        else {
            return Err(McpError::Startup {
                server: server.name.clone(),
                message: "internal transport mismatch".to_owned(),
            });
        };
        let mut headers = HashMap::new();
        for (header, source) in headers_from {
            let name =
                HeaderName::from_bytes(header.as_bytes()).map_err(|error| McpError::Startup {
                    server: server.name.clone(),
                    message: format!("invalid header name {header:?}: {error}"),
                })?;
            let secret =
                SecretString::from(std::env::var(source).map_err(|_| McpError::Startup {
                    server: server.name.clone(),
                    message: format!("required environment variable {source:?} is not set"),
                })?);
            let mut value =
                HeaderValue::from_str(secret.expose_secret()).map_err(|_| McpError::Startup {
                    server: server.name.clone(),
                    message: format!("environment variable {source:?} is not a valid HTTP header"),
                })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone())
            .custom_headers(headers)
            .max_sse_event_size(self.config.max_sse_event_bytes)
            .reinit_on_expired_session(true);
        let mut retry_policy = ExponentialBackoff::default();
        retry_policy.max_times = Some(self.config.reconnect_max_attempts as usize);
        retry_policy.base_duration = self.config.reconnect_base_delay;
        transport_config.retry_config = Arc::new(retry_policy);
        if let Some(source) = bearer_token_env {
            let token =
                SecretString::from(std::env::var(source).map_err(|_| McpError::Startup {
                    server: server.name.clone(),
                    message: format!("bearer token environment variable {source:?} is not set"),
                })?);
            transport_config = transport_config.auth_header(token.expose_secret().to_owned());
        }
        let transport = StreamableHttpClientTransport::from_config(transport_config);
        let service = timeout(self.config.startup_timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::StartupTimeout {
                server: server.name.clone(),
                secs: self.config.startup_timeout.as_secs(),
            })?
            .map_err(|error| McpError::Startup {
                server: server.name.clone(),
                message: error.to_string(),
            })?;
        Ok((service, None))
    }

    async fn connect_oauth_from_store(
        &self,
        server: &McpServerConfig,
    ) -> Result<(ClientService, Option<JoinHandle<()>>), McpError> {
        let McpTransportConfig::StreamableHttp {
            url,
            oauth: Some(_),
            ..
        } = &server.transport
        else {
            return Err(McpError::Startup {
                server: server.name.clone(),
                message: "internal OAuth transport mismatch".to_owned(),
            });
        };
        let (mut manager, store) = manager_with_store(&server.name, url).await?;
        let restored = timeout(self.config.startup_timeout, manager.initialize_from_store())
            .await
            .map_err(|_| McpError::StartupTimeout {
                server: server.name.clone(),
                secs: self.config.startup_timeout.as_secs(),
            })?
            .map_err(|error| oauth_error(&server.name, error))?;
        if !restored {
            return Err(McpError::OAuthReauthRequired {
                server: server.name.clone(),
                message: "no reusable credentials were found in the OS keyring".to_owned(),
            });
        }
        match timeout(self.config.startup_timeout, manager.get_access_token()).await {
            Ok(Ok(_)) => {}
            Ok(Err(error @ (AuthError::TokenRefreshRejected(_) | AuthError::TokenExpired))) => {
                if let Err(clear_error) = store.clear().await {
                    tracing::warn!(server = %server.name, error = %clear_error, "could not clear rejected OAuth credentials");
                }
                return Err(oauth_error(&server.name, error));
            }
            Ok(Err(error)) => return Err(oauth_error(&server.name, error)),
            Err(_) => {
                return Err(McpError::StartupTimeout {
                    server: server.name.clone(),
                    secs: self.config.startup_timeout.as_secs(),
                });
            }
        }
        self.connect_authorized_http(server, manager).await
    }

    async fn connect_authorized_http(
        &self,
        server: &McpServerConfig,
        manager: AuthorizationManager,
    ) -> Result<(ClientService, Option<JoinHandle<()>>), McpError> {
        let McpTransportConfig::StreamableHttp {
            url,
            headers_from,
            oauth: Some(_),
            ..
        } = &server.transport
        else {
            return Err(McpError::Startup {
                server: server.name.clone(),
                message: "internal OAuth transport mismatch".to_owned(),
            });
        };
        let headers = resolve_http_headers(&server.name, headers_from)?;
        let http_client = reqwest13::Client::builder()
            .redirect(reqwest13::redirect::Policy::none())
            .pool_max_idle_per_host(0)
            .timeout(self.config.tool_timeout)
            .build()
            .map_err(|error| McpError::Startup {
                server: server.name.clone(),
                message: format!("could not build OAuth HTTP client: {error}"),
            })?;
        let auth_client = AuthClient::new(http_client, manager);
        let mut transport_config = StreamableHttpClientTransportConfig::with_uri(url.clone())
            .custom_headers(headers)
            .max_sse_event_size(self.config.max_sse_event_bytes)
            .reinit_on_expired_session(true);
        let mut retry_policy = ExponentialBackoff::default();
        retry_policy.max_times = Some(self.config.reconnect_max_attempts as usize);
        retry_policy.base_duration = self.config.reconnect_base_delay;
        transport_config.retry_config = Arc::new(retry_policy);
        let transport = StreamableHttpClientTransport::with_client(auth_client, transport_config);
        let service = timeout(self.config.startup_timeout, ().serve(transport))
            .await
            .map_err(|_| McpError::StartupTimeout {
                server: server.name.clone(),
                secs: self.config.startup_timeout.as_secs(),
            })?
            .map_err(|error| McpError::Startup {
                server: server.name.clone(),
                message: error.to_string(),
            })?;
        Ok((service, None))
    }

    fn rebuild_tool_index(&mut self) {
        self.tools = self
            .servers
            .values()
            .flat_map(|server| server.tools.iter().cloned())
            .collect();
        assign_function_names(&mut self.tools);
    }

    fn set_state(&mut self, server_name: &str, state: McpConnectionState, notice: UiNotice) {
        if let Some(server) = self.servers.get_mut(server_name) {
            server.state = state;
            server.notice =
                redact_mcp_notice(&server.config, notice, |name| std::env::var(name).ok());
        }
    }
}

fn redact_mcp_notice(
    server: &McpServerConfig,
    notice: UiNotice,
    lookup: impl Fn(&str) -> Option<String>,
) -> UiNotice {
    let UiNotice::ExternalError { detail } = notice else {
        return notice;
    };
    let secrets = mcp_secret_values(server, lookup);
    UiNotice::external(redact_secret_values(
        detail,
        secrets.iter().map(SecretString::expose_secret),
    ))
}

fn mcp_secret_values(
    server: &McpServerConfig,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<SecretString> {
    let variable_names = match &server.transport {
        McpTransportConfig::Stdio { env_from, .. } => env_from.values().collect::<Vec<_>>(),
        McpTransportConfig::StreamableHttp {
            bearer_token_env,
            headers_from,
            ..
        } => bearer_token_env
            .iter()
            .chain(headers_from.values())
            .collect::<Vec<_>>(),
    };
    variable_names
        .into_iter()
        .filter_map(|name| lookup(name))
        .map(SecretString::from)
        .collect()
}

fn redact_mcp_error(
    server: &McpServerConfig,
    error: McpError,
    lookup: impl Fn(&str) -> Option<String>,
) -> McpError {
    let secrets = mcp_secret_values(server, lookup);
    let redact = |value: String| {
        redact_secret_values(value, secrets.iter().map(SecretString::expose_secret))
    };
    match error {
        McpError::NotConnected { server, reason } => McpError::NotConnected {
            server,
            reason: redact(reason),
        },
        McpError::Startup { server, message } => McpError::Startup {
            server,
            message: redact(message),
        },
        McpError::Protocol { server, message } => McpError::Protocol {
            server,
            message: redact(message),
        },
        McpError::PermissionDenied { reason } => McpError::PermissionDenied {
            reason: redact(reason),
        },
        McpError::OAuth { server, message } => McpError::OAuth {
            server,
            message: redact(message),
        },
        McpError::OAuthReauthRequired { server, message } => McpError::OAuthReauthRequired {
            server,
            message: redact(message),
        },
        McpError::OAuthCallback { server, message } => McpError::OAuthCallback {
            server,
            message: redact(message),
        },
        other => other,
    }
}

impl Drop for McpManager {
    fn drop(&mut self) {
        for server in self.servers.values_mut() {
            if let Some(service) = server.service.take() {
                service.cancellation_token().cancel();
            }
            if let Some(task) = server.stderr_task.take() {
                task.abort();
            }
        }
    }
}

fn convert_tool(server: &str, tool: Tool) -> McpTool {
    let annotations = tool.annotations.as_ref();
    McpTool {
        server: server.to_owned(),
        name: tool.name.into_owned(),
        function_name: String::new(),
        title: tool.title,
        description: tool.description.map(|description| description.into_owned()),
        input_schema: Value::Object((*tool.input_schema).clone()),
        read_only_hint: annotations.and_then(|value| value.read_only_hint),
        destructive_hint: annotations.and_then(|value| value.destructive_hint),
        open_world_hint: annotations.and_then(|value| value.open_world_hint),
    }
}

fn resolve_http_headers(
    server: &str,
    headers_from: &BTreeMap<String, String>,
) -> Result<HashMap<HeaderName, HeaderValue>, McpError> {
    let mut headers = HashMap::new();
    for (header, source) in headers_from {
        let name =
            HeaderName::from_bytes(header.as_bytes()).map_err(|error| McpError::Startup {
                server: server.to_owned(),
                message: format!("invalid header name {header:?}: {error}"),
            })?;
        let secret = SecretString::from(std::env::var(source).map_err(|_| McpError::Startup {
            server: server.to_owned(),
            message: format!("required environment variable {source:?} is not set"),
        })?);
        let mut value =
            HeaderValue::from_str(secret.expose_secret()).map_err(|_| McpError::Startup {
                server: server.to_owned(),
                message: format!("environment variable {source:?} is not a valid HTTP header"),
            })?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

fn bounded_call_output(
    result: rmcp::model::CallToolResult,
    max_bytes: usize,
    server: &str,
) -> Result<McpCallOutput, McpError> {
    let is_error = result.is_error.unwrap_or(false);
    let value = serde_json::to_value(result).map_err(|error| McpError::Protocol {
        server: server.to_owned(),
        message: format!("could not serialize tool result: {error}"),
    })?;
    let mut content = serde_json::to_string(&value).map_err(|error| McpError::Protocol {
        server: server.to_owned(),
        message: format!("could not encode tool result: {error}"),
    })?;
    let truncated = content.len() > max_bytes;
    if truncated {
        const SUFFIX: &str = "…[MCP result truncated by client]";
        let content_limit = max_bytes.saturating_sub(SUFFIX.len());
        let mut end = content_limit.min(content.len());
        while !content.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        content.truncate(end);
        if SUFFIX.len() <= max_bytes {
            content.push_str(SUFFIX);
        }
    }
    Ok(McpCallOutput {
        content,
        is_error,
        truncated,
    })
}

fn drain_stderr(server: String, mut stderr: tokio::process::ChildStderr) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) => break,
                Ok(read) => {
                    tracing::debug!(
                        server = %server,
                        bytes = read,
                        "MCP server wrote to stderr"
                    );
                }
                Err(error) => {
                    tracing::debug!(server = %server, error = %error, "MCP stderr reader stopped");
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use rmcp::model::CallToolResult;

    use super::*;

    #[test]
    fn result_is_bounded_before_it_can_reach_history_or_tui() -> Result<(), McpError> {
        let result = CallToolResult::structured(serde_json::json!({
            "data": "🦀".repeat(4096)
        }));
        let output = bounded_call_output(result, 1024, "fixture")?;
        assert!(output.truncated);
        assert!(output.content.len() <= 1024);
        assert!(output.content.ends_with("[MCP result truncated by client]"));
        assert!(std::str::from_utf8(output.content.as_bytes()).is_ok());
        Ok(())
    }

    #[test]
    fn configured_mcp_secrets_are_removed_from_user_visible_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut headers_from = BTreeMap::new();
        headers_from.insert("X-Private-Key".to_owned(), "MCP_HEADER_SECRET".to_owned());
        let server = McpServerConfig {
            name: "fixture".to_owned(),
            enabled: true,
            required: false,
            transport: McpTransportConfig::StreamableHttp {
                url: "https://mcp.example.invalid/runtime".to_owned(),
                bearer_token_env: Some("MCP_BEARER_SECRET".to_owned()),
                headers_from,
                oauth: None,
            },
            permissions: crate::mcp::McpPermissionConfig::default(),
        };
        let bearer = "mcp-fake-bearer-21";
        let header = "mcp-fake-header-42";
        let notice = redact_mcp_notice(
            &server,
            UiNotice::external(format!("Bearer {bearer}; X-Private-Key={header}")),
            |name| match name {
                "MCP_BEARER_SECRET" => Some(bearer.to_owned()),
                "MCP_HEADER_SECRET" => Some(header.to_owned()),
                _ => None,
            },
        );

        let UiNotice::ExternalError { detail } = notice else {
            return Err("external errors must remain typed after redaction".into());
        };
        assert_eq!(detail.matches(crate::redaction::REDACTED).count(), 2);
        assert!(!detail.contains(bearer));
        assert!(!detail.contains(header));

        let error = redact_mcp_error(
            &server,
            McpError::Protocol {
                server: "fixture".to_owned(),
                message: format!("upstream echoed {bearer} and {header}"),
            },
            |name| match name {
                "MCP_BEARER_SECRET" => Some(bearer.to_owned()),
                "MCP_HEADER_SECRET" => Some(header.to_owned()),
                _ => None,
            },
        );
        let rendered = error.to_string();
        assert!(!rendered.contains(bearer));
        assert!(!rendered.contains(header));
        Ok(())
    }

    fn switch_test_config(global_enabled: bool, server_enabled: bool) -> McpConfig {
        McpConfig {
            enabled: global_enabled,
            servers: vec![McpServerConfig {
                name: "fixture".to_owned(),
                enabled: server_enabled,
                required: false,
                transport: McpTransportConfig::StreamableHttp {
                    url: "https://mcp.example.invalid/runtime".to_owned(),
                    bearer_token_env: None,
                    headers_from: BTreeMap::new(),
                    oauth: None,
                },
                permissions: crate::mcp::McpPermissionConfig::default(),
            }],
            ..McpConfig::default()
        }
    }

    #[tokio::test]
    async fn runtime_switch_revokes_server_without_touching_config_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut manager = McpManager::new(switch_test_config(true, true))?;
        manager.set_enabled("fixture", false).await?;
        let disabled = manager
            .snapshots()
            .into_iter()
            .next()
            .ok_or("missing fixture snapshot")?;
        assert!(!disabled.enabled);
        assert_eq!(disabled.state, McpConnectionState::Disabled);
        assert!(manager.tools().is_empty());
        assert!(matches!(
            manager.connect("fixture").await,
            Err(McpError::NotConnected { .. })
        ));

        manager.set_enabled("fixture", true).await?;
        let enabled = manager
            .snapshots()
            .into_iter()
            .next()
            .ok_or("missing fixture snapshot")?;
        assert!(enabled.enabled);
        assert_eq!(enabled.state, McpConnectionState::Disconnected);
        Ok(())
    }

    #[tokio::test]
    async fn global_mcp_disable_wins_over_per_server_switch()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut manager = McpManager::new(switch_test_config(false, false))?;
        assert!(matches!(
            manager.set_enabled("fixture", true).await,
            Err(McpError::RuntimeDisabled)
        ));
        let snapshot = manager
            .snapshots()
            .into_iter()
            .next()
            .ok_or("missing fixture snapshot")?;
        assert!(!snapshot.runtime_available);
        assert!(!snapshot.enabled);
        assert_eq!(snapshot.state, McpConnectionState::Disabled);
        Ok(())
    }

    #[tokio::test]
    async fn disabled_server_cannot_start_oauth() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = switch_test_config(true, false);
        config.servers[0].transport = McpTransportConfig::StreamableHttp {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            bearer_token_env: None,
            headers_from: BTreeMap::new(),
            oauth: Some(crate::mcp::McpOAuthConfig {
                client_id: None,
                scopes: Vec::new(),
                callback_port: 0,
            }),
        };
        let mut manager = McpManager::new(config)?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            manager.begin_oauth("fixture"),
        )
        .await?;
        assert!(matches!(result, Err(McpError::NotConnected { .. })));
        Ok(())
    }
}
