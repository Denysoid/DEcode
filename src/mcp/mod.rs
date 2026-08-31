mod client;
mod oauth;
mod permissions;
mod types;

pub use client::McpManager;
pub use permissions::{McpPermissionDecision, evaluate_permission};
pub use types::{
    McpApprovalMode, McpCallOutput, McpConfig, McpConnectionState, McpOAuthConfig, McpOAuthPrompt,
    McpPermissionConfig, McpServerConfig, McpServerSnapshot, McpTool, McpTransportConfig,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP is disabled globally in the trusted configuration")]
    RuntimeDisabled,
    #[error("MCP server {server:?} is not configured")]
    UnknownServer { server: String },
    #[error("MCP tool {tool:?} is not available on server {server:?}")]
    UnknownTool { server: String, tool: String },
    #[error("MCP server {server:?} is not connected: {reason}")]
    NotConnected { server: String, reason: String },
    #[error("failed to start MCP server {server:?}: {message}")]
    Startup { server: String, message: String },
    #[error("MCP server {server:?} startup timed out after {secs}s")]
    StartupTimeout { server: String, secs: u64 },
    #[error("MCP operation {operation:?} on server {server:?} timed out after {secs}s")]
    OperationTimeout {
        server: String,
        operation: String,
        secs: u64,
    },
    #[error("MCP protocol error from server {server:?}: {message}")]
    Protocol { server: String, message: String },
    #[error("MCP tool call denied: {reason}")]
    PermissionDenied { reason: String },
    #[error("MCP tool arguments must be a JSON object")]
    InvalidArguments,
    #[error("MCP server {server:?} panicked while handling {operation:?}")]
    DependencyPanic { server: String, operation: String },
    #[error("MCP OAuth error for server {server:?}: {message}")]
    OAuth { server: String, message: String },
    #[error("MCP server {server:?} requires OAuth authorization: {message}")]
    OAuthReauthRequired { server: String, message: String },
    #[error("MCP OAuth callback error for server {server:?}: {message}")]
    OAuthCallback { server: String, message: String },
}
