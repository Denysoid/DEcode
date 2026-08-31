use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    lsp::LspServerConfig,
    mcp::{McpApprovalMode, McpOAuthConfig, McpServerConfig, McpTransportConfig},
};

const MANAGED_NAMESPACE: &str = "user-managed";
const MAX_CONNECTION_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum ManagedConnectionError {
    #[error("managed connection name is invalid: {0}")]
    InvalidName(String),
    #[error("managed connection could not be encoded: {0}")]
    Encode(#[from] toml::ser::Error),
    #[error("managed connection I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("managed connection file exceeds the {MAX_CONNECTION_FILE_BYTES} byte limit")]
    TooLarge,
    #[error("managed connection directory is a symbolic link: {0}")]
    Symlink(PathBuf),
}

#[derive(Clone, Debug)]
pub struct ManagedConnectionStore {
    root: PathBuf,
}

impl ManagedConnectionStore {
    #[must_use]
    pub fn from_skills_dir(skills_dir: &Path) -> Self {
        let integration_root = skills_dir.parent().unwrap_or(skills_dir);
        Self {
            root: integration_root
                .join("plugin-connections")
                .join(MANAGED_NAMESPACE),
        }
    }

    pub fn save_mcp(&self, server: &McpServerConfig) -> Result<PathBuf, ManagedConnectionError> {
        validate_file_name(&server.name)?;
        let record = McpContribution {
            servers: vec![SerializableMcpServer::from(server)],
        };
        self.save("mcp", &server.name, &record)
    }

    pub fn save_lsp(&self, server: &LspServerConfig) -> Result<PathBuf, ManagedConnectionError> {
        validate_file_name(&server.name)?;
        let record = LspContribution {
            servers: vec![SerializableLspServer::from(server)],
        };
        self.save("lsp", &server.name, &record)
    }

    fn save<T: Serialize>(
        &self,
        kind: &str,
        name: &str,
        value: &T,
    ) -> Result<PathBuf, ManagedConnectionError> {
        let directory = self.root.join(kind);
        if let Some(parent) = self.root.parent() {
            ensure_real_directory(parent)?;
        }
        ensure_real_directory(&self.root)?;
        ensure_real_directory(&directory)?;
        let bytes = toml::to_string_pretty(value)?.into_bytes();
        if bytes.len() > MAX_CONNECTION_FILE_BYTES {
            return Err(ManagedConnectionError::TooLarge);
        }
        let mut path = directory.join(connection_file_name(name));
        if path.exists() && stored_connection_name(&path).as_deref() != Some(name) {
            path = directory.join(collision_file_name(name));
        }
        atomic_write(&path, &bytes)?;
        Ok(path)
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), ManagedConnectionError> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(ManagedConnectionError::Symlink(path.to_path_buf()));
        }
        return Ok(());
    }
    fs::create_dir_all(path).map_err(|source| ManagedConnectionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| ManagedConnectionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ManagedConnectionError::Symlink(path.to_path_buf()));
    }
    Ok(())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ManagedConnectionError> {
    let parent = path.parent().ok_or_else(|| ManagedConnectionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"),
    })?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| ManagedConnectionError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| ManagedConnectionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| ManagedConnectionError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn validate_file_name(name: &str) -> Result<(), ManagedConnectionError> {
    if name.trim().is_empty()
        || name.len() > 128
        || name.contains(char::is_control)
        || name.contains(['/', '\\'])
    {
        return Err(ManagedConnectionError::InvalidName(name.to_owned()));
    }
    Ok(())
}

fn connection_file_name(name: &str) -> String {
    let stem = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let stem = stem.trim_matches('-');
    let stem = if stem.is_empty() { "connection" } else { stem };
    let digest = format!("{:x}", Sha256::digest(name.to_ascii_lowercase().as_bytes()));
    format!("{}-{}.toml", &stem[..stem.len().min(40)], &digest[..12])
}

fn collision_file_name(name: &str) -> String {
    let legacy = connection_file_name(name);
    let stem = legacy.strip_suffix(".toml").unwrap_or(&legacy);
    let digest = format!(
        "{:x}",
        Sha256::digest([b"case-sensitive:", name.as_bytes()].concat())
    );
    format!("{stem}-{}.toml", &digest[..12])
}

fn stored_connection_name(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let value = toml::from_str::<toml::Value>(&text).ok()?;
    value
        .get("servers")?
        .as_array()?
        .first()?
        .get("name")?
        .as_str()
        .map(str::to_owned)
}

#[derive(Serialize)]
struct McpContribution {
    servers: Vec<SerializableMcpServer>,
}

#[derive(Serialize)]
struct SerializableMcpServer {
    name: String,
    enabled: bool,
    required: bool,
    transport: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    env_from: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_directory: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bearer_token_env: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    headers_from: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth: Option<SerializableMcpOAuth>,
    approval: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    enabled_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    disabled_tools: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    trusted_read_only_tools: Vec<String>,
}

#[derive(Serialize)]
struct SerializableMcpOAuth {
    #[serde(skip_serializing_if = "Option::is_none")]
    client_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<String>,
    callback_port: u16,
}

impl From<&McpOAuthConfig> for SerializableMcpOAuth {
    fn from(oauth: &McpOAuthConfig) -> Self {
        Self {
            client_id: oauth.client_id.clone(),
            scopes: oauth.scopes.clone(),
            callback_port: oauth.callback_port,
        }
    }
}

impl From<&McpServerConfig> for SerializableMcpServer {
    fn from(server: &McpServerConfig) -> Self {
        let transport = match server.transport {
            McpTransportConfig::Stdio { .. } => "stdio",
            McpTransportConfig::StreamableHttp { .. } => "http",
        };
        let mut result = Self {
            name: server.name.clone(),
            enabled: server.enabled,
            required: server.required,
            transport,
            command: None,
            args: Vec::new(),
            env_from: BTreeMap::new(),
            working_directory: None,
            url: None,
            bearer_token_env: None,
            headers_from: BTreeMap::new(),
            oauth: None,
            approval: match server.permissions.approval {
                McpApprovalMode::Always => "always",
                McpApprovalMode::Writes => "writes",
                McpApprovalMode::Never => "never",
            },
            enabled_tools: server.permissions.enabled_tools.iter().cloned().collect(),
            disabled_tools: server.permissions.disabled_tools.iter().cloned().collect(),
            trusted_read_only_tools: server
                .permissions
                .trusted_read_only_tools
                .iter()
                .cloned()
                .collect(),
        };
        match &server.transport {
            McpTransportConfig::Stdio {
                command,
                args,
                env_from,
                working_directory,
            } => {
                result.command = Some(command.clone());
                result.args.clone_from(args);
                result.env_from.clone_from(env_from);
                result.working_directory.clone_from(working_directory);
            }
            McpTransportConfig::StreamableHttp {
                url,
                bearer_token_env,
                headers_from,
                oauth,
            } => {
                result.url = Some(url.clone());
                result.bearer_token_env.clone_from(bearer_token_env);
                result.headers_from.clone_from(headers_from);
                result.oauth = oauth.as_ref().map(SerializableMcpOAuth::from);
            }
        }
        result
    }
}

#[derive(Serialize)]
struct LspContribution {
    servers: Vec<SerializableLspServer>,
}

#[derive(Serialize)]
struct SerializableLspServer {
    name: String,
    enabled: bool,
    required: bool,
    auto_start: bool,
    command: String,
    args: Vec<String>,
    language_id: String,
    extensions: Vec<String>,
    root_markers: Vec<String>,
}

impl From<&LspServerConfig> for SerializableLspServer {
    fn from(server: &LspServerConfig) -> Self {
        Self {
            name: server.name.clone(),
            enabled: server.enabled,
            required: server.required,
            auto_start: server.auto_start,
            command: server.command.clone(),
            args: server.args.clone(),
            language_id: server.language_id.clone(),
            extensions: server.extensions.clone(),
            root_markers: server.root_markers.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::{ManagedConnectionError, ManagedConnectionStore};
    use crate::lsp::LspServerConfig;
    use crate::mcp::{McpApprovalMode, McpPermissionConfig, McpServerConfig, McpTransportConfig};

    fn lsp_server(name: &str) -> LspServerConfig {
        LspServerConfig {
            name: name.to_owned(),
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

    #[test]
    fn mcp_connection_is_written_without_secret_values() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let store = ManagedConnectionStore::from_skills_dir(&directory.path().join("skills"));
        let path = store.save_mcp(&McpServerConfig {
            name: "local docs".to_owned(),
            enabled: true,
            required: false,
            transport: McpTransportConfig::StreamableHttp {
                url: "https://example.test/mcp".to_owned(),
                bearer_token_env: Some("DOCS_TOKEN".to_owned()),
                headers_from: BTreeMap::new(),
                oauth: None,
            },
            permissions: McpPermissionConfig {
                approval: McpApprovalMode::Always,
                enabled_tools: BTreeSet::new(),
                disabled_tools: BTreeSet::new(),
                trusted_read_only_tools: BTreeSet::new(),
            },
        })?;
        let text = std::fs::read_to_string(path)?;
        assert!(text.contains("bearer_token_env = \"DOCS_TOKEN\""));
        assert!(!text.contains("secret-value"));
        assert!(text.contains("approval = \"always\""));
        Ok(())
    }

    #[test]
    fn mcp_connection_persists_oauth_and_permission_policy()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let store = ManagedConnectionStore::from_skills_dir(&directory.path().join("skills"));
        let path = store.save_mcp(&McpServerConfig {
            name: "oauth docs".to_owned(),
            enabled: true,
            required: true,
            transport: McpTransportConfig::StreamableHttp {
                url: "https://example.test/mcp".to_owned(),
                bearer_token_env: None,
                headers_from: BTreeMap::from([("X-Tenant".to_owned(), "TENANT_ID".to_owned())]),
                oauth: Some(crate::mcp::McpOAuthConfig {
                    client_id: Some("decode-client".to_owned()),
                    scopes: vec!["tools.read".to_owned()],
                    callback_port: 4242,
                }),
            },
            permissions: McpPermissionConfig {
                approval: McpApprovalMode::Writes,
                enabled_tools: BTreeSet::from(["search".to_owned()]),
                disabled_tools: BTreeSet::from(["delete".to_owned()]),
                trusted_read_only_tools: BTreeSet::from(["search".to_owned()]),
            },
        })?;
        let text = std::fs::read_to_string(path)?;
        assert!(text.contains("client_id = \"decode-client\""));
        assert!(text.contains("callback_port = 4242"));
        assert!(text.contains("headers_from"));
        assert!(text.contains("enabled_tools = [\"search\"]"));
        assert!(text.contains("disabled_tools = [\"delete\"]"));
        assert!(text.contains("trusted_read_only_tools = [\"search\"]"));
        Ok(())
    }

    #[test]
    fn case_distinct_lsp_names_do_not_overwrite_each_other()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let store = ManagedConnectionStore::from_skills_dir(&directory.path().join("skills"));

        let uppercase = store.save_lsp(&lsp_server("Rust"))?;
        let lowercase = store.save_lsp(&lsp_server("rust"))?;
        let uppercase_update = store.save_lsp(&lsp_server("Rust"))?;

        assert_ne!(uppercase, lowercase);
        assert_eq!(uppercase_update, uppercase);
        assert!(std::fs::read_to_string(uppercase)?.contains("name = \"Rust\""));
        assert!(std::fs::read_to_string(lowercase)?.contains("name = \"rust\""));
        Ok(())
    }

    #[test]
    fn save_rejects_a_symlinked_connection_root() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let redirected = tempfile::tempdir()?;
        let connection_root = directory.path().join("plugin-connections");
        #[cfg(unix)]
        std::os::unix::fs::symlink(redirected.path(), &connection_root)?;
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(&connection_root)
                .arg(redirected.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            assert!(status.success(), "could not create test junction");
        }
        let store = ManagedConnectionStore::from_skills_dir(&directory.path().join("skills"));

        assert!(matches!(
            store.save_lsp(&lsp_server("rust")),
            Err(ManagedConnectionError::Symlink(path)) if path == connection_root
        ));
        assert!(!redirected.path().join("user-managed").exists());
        Ok(())
    }
}
