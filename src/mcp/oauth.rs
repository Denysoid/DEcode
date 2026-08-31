use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use rmcp::transport::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore,
    StoredCredentials,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::timeout,
};

use super::{McpError, McpOAuthConfig};

const CALLBACK_PATH: &str = "/oauth/callback";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_CALLBACK_REQUEST_BYTES: usize = 16 * 1024;

pub(crate) struct PendingOAuth {
    pub session: AuthorizationSession,
    pub callback_rx: oneshot::Receiver<Result<String, OAuthCallbackError>>,
    pub redirect_uri: String,
}

#[derive(Debug, Error)]
pub(crate) enum OAuthCallbackError {
    #[error("callback accept failed: {message}")]
    Accept { message: String },
    #[error("authorization callback timed out after 5 minutes")]
    AuthorizationTimeout,
    #[error("callback HTTP request timed out")]
    RequestTimeout,
    #[error("callback read failed: {message}")]
    Read { message: String },
    #[error("callback HTTP request exceeded 16 KiB")]
    RequestTooLarge,
    #[error("callback HTTP request was not UTF-8")]
    InvalidUtf8,
    #[error("callback HTTP request line was malformed")]
    MalformedRequest,
    #[error("callback URL was malformed: {message}")]
    MalformedUrl { message: String },
    #[error("callback path did not match the registered redirect URI")]
    PathMismatch,
    #[error("callback URL is missing code or state")]
    MissingParameters,
    #[error("callback response failed: {message}")]
    Response { message: String },
}

#[derive(Clone)]
pub(crate) struct KeyringCredentialStore {
    service: Arc<str>,
    account: Arc<str>,
}

impl KeyringCredentialStore {
    pub(crate) fn new(server_name: &str, server_url: &str) -> Self {
        let digest = Sha256::digest(format!("{server_name}\0{server_url}").as_bytes());
        let account = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        Self {
            service: Arc::from("decode-mcp-oauth"),
            account: Arc::from(account),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|error| AuthError::InternalError(format!("OS keyring unavailable: {error}")))
    }
}

#[async_trait]
impl CredentialStore for KeyringCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let entry = store.entry()?;
            match entry.get_password() {
                Ok(encoded) => serde_json::from_str(&encoded).map(Some).map_err(|error| {
                    AuthError::InternalError(format!(
                        "stored OAuth credentials are invalid: {error}"
                    ))
                }),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(AuthError::InternalError(format!(
                    "could not read OAuth credentials from OS keyring: {error}"
                ))),
            }
        })
        .await
        .map_err(|error| AuthError::InternalError(format!("keyring task failed: {error}")))?
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let encoded = serde_json::to_string(&credentials).map_err(|error| {
            AuthError::InternalError(format!("could not encode OAuth credentials: {error}"))
        })?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.entry()?.set_password(&encoded).map_err(|error| {
                AuthError::InternalError(format!(
                    "could not save OAuth credentials to OS keyring: {error}"
                ))
            })
        })
        .await
        .map_err(|error| AuthError::InternalError(format!("keyring task failed: {error}")))?
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let entry = store.entry()?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(AuthError::InternalError(format!(
                    "could not clear OAuth credentials from OS keyring: {error}"
                ))),
            }
        })
        .await
        .map_err(|error| AuthError::InternalError(format!("keyring task failed: {error}")))?
    }
}

pub(crate) async fn manager_with_store(
    server_name: &str,
    server_url: &str,
) -> Result<(AuthorizationManager, KeyringCredentialStore), McpError> {
    let store = KeyringCredentialStore::new(server_name, server_url);
    let mut manager = AuthorizationManager::new(server_url)
        .await
        .map_err(|error| oauth_error(server_name, error))?;
    manager.set_credential_store(store.clone());
    Ok((manager, store))
}

pub(crate) async fn begin_authorization(
    server_name: &str,
    server_url: &str,
    oauth: &McpOAuthConfig,
) -> Result<(PendingOAuth, String), McpError> {
    let (listener, redirect_uri, callback_tx, callback_rx) =
        bind_callback(server_name, oauth.callback_port).await?;

    let (mut manager, _store) = manager_with_store(server_name, server_url).await?;
    let resolution = manager
        .resolve_metadata()
        .await
        .map_err(|error| oauth_error(server_name, error))?;
    manager.set_metadata(resolution.metadata);
    let mut request = AuthorizationRequest::new(redirect_uri.clone())
        .with_client_name("DEcode by denysoid MCP Client")
        .with_scopes(oauth.scopes.clone());
    if let Some(client_id) = &oauth.client_id {
        request = request.with_preregistered_client(client_id.clone());
    }
    let session = AuthorizationSession::new(manager, request)
        .await
        .map_err(|(_, error)| oauth_error(server_name, error))?;
    let authorization_url = session.get_authorization_url().to_owned();
    spawn_callback_listener(server_name.to_owned(), listener, callback_tx);
    Ok((
        PendingOAuth {
            session,
            callback_rx,
            redirect_uri,
        },
        authorization_url,
    ))
}

pub(crate) fn oauth_error(server: &str, error: AuthError) -> McpError {
    match error {
        AuthError::AuthorizationRequired
        | AuthError::TokenRefreshRejected(_)
        | AuthError::TokenExpired => McpError::OAuthReauthRequired {
            server: server.to_owned(),
            message: error.to_string(),
        },
        other => McpError::OAuth {
            server: server.to_owned(),
            message: other.to_string(),
        },
    }
}

async fn bind_callback(
    server_name: &str,
    requested_port: u16,
) -> Result<
    (
        TcpListener,
        String,
        oneshot::Sender<Result<String, OAuthCallbackError>>,
        oneshot::Receiver<Result<String, OAuthCallbackError>>,
    ),
    McpError,
> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, requested_port))
        .await
        .map_err(|error| McpError::OAuthCallback {
            server: server_name.to_owned(),
            message: format!("could not bind loopback callback: {error}"),
        })?;
    let address = listener
        .local_addr()
        .map_err(|error| McpError::OAuthCallback {
            server: server_name.to_owned(),
            message: format!("could not inspect callback address: {error}"),
        })?;
    let (callback_tx, callback_rx) = oneshot::channel();
    Ok((
        listener,
        format!("http://127.0.0.1:{}{CALLBACK_PATH}", address.port()),
        callback_tx,
        callback_rx,
    ))
}

fn spawn_callback_listener(
    server: String,
    listener: TcpListener,
    sender: oneshot::Sender<Result<String, OAuthCallbackError>>,
) {
    tokio::spawn(async move {
        let callback = async {
            let (stream, _) =
                listener
                    .accept()
                    .await
                    .map_err(|error| OAuthCallbackError::Accept {
                        message: error.to_string(),
                    })?;
            read_callback(stream).await
        };
        let result = match timeout(CALLBACK_TIMEOUT, callback).await {
            Ok(result) => result,
            Err(_) => Err(OAuthCallbackError::AuthorizationTimeout),
        };
        if sender.send(result).is_err() {
            tracing::warn!(server = %server, "OAuth callback sender disappeared");
        }
    });
}

async fn read_callback(mut stream: TcpStream) -> Result<String, OAuthCallbackError> {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .map_err(|_| OAuthCallbackError::RequestTimeout)?
            .map_err(|error| OAuthCallbackError::Read {
                message: error.to_string(),
            })?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..read]);
        if request.len() > MAX_CALLBACK_REQUEST_BYTES {
            return Err(OAuthCallbackError::RequestTooLarge);
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&request).map_err(|_| OAuthCallbackError::InvalidUtf8)?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or(OAuthCallbackError::MalformedRequest)?;
    let parsed = reqwest::Url::parse(&format!("http://127.0.0.1{target}")).map_err(|error| {
        OAuthCallbackError::MalformedUrl {
            message: error.to_string(),
        }
    })?;
    if parsed.path() != CALLBACK_PATH {
        return Err(OAuthCallbackError::PathMismatch);
    }
    let has_code = parsed.query_pairs().any(|(key, _)| key == "code");
    let has_state = parsed.query_pairs().any(|(key, _)| key == "state");
    if !has_code || !has_state {
        return Err(OAuthCallbackError::MissingParameters);
    }
    let body = "Authorization complete. You can close this tab and return to DEcode by denysoid.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|error| OAuthCallbackError::Response {
            message: error.to_string(),
        })?;
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn callback_listener_accepts_only_bounded_causal_callback()
    -> Result<(), Box<dyn std::error::Error>> {
        let (listener, redirect, sender, receiver) = bind_callback("fixture", 0).await?;
        spawn_callback_listener("fixture".to_owned(), listener, sender);
        let target = reqwest::Url::parse(&redirect)?;
        let mut stream =
            TcpStream::connect(("127.0.0.1", target.port().unwrap_or_default())).await?;
        stream
            .write_all(
                b"GET /oauth/callback?code=abc&state=csrf HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            )
            .await?;
        let callback = timeout(Duration::from_secs(2), receiver).await???;
        assert!(callback.contains("code=abc"));
        assert!(callback.contains("state=csrf"));
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn callback_timeout_covers_the_whole_slow_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let (listener, redirect, sender, mut receiver) = bind_callback("fixture", 0).await?;
        spawn_callback_listener("fixture".to_owned(), listener, sender);
        let target = reqwest::Url::parse(&redirect)?;
        let mut stream =
            TcpStream::connect(("127.0.0.1", target.port().unwrap_or_default())).await?;
        tokio::task::yield_now().await;

        let mut received = None;
        for _ in 0..80 {
            if let Ok(result) = receiver.try_recv() {
                received = Some(result);
                break;
            }
            let _ = stream.write_all(b"x").await;
            tokio::time::advance(Duration::from_secs(4)).await;
            tokio::task::yield_now().await;
        }
        let result = match received {
            Some(result) => result,
            None => {
                tokio::time::advance(Duration::from_secs(6)).await;
                receiver.await?
            }
        };
        assert!(matches!(
            result,
            Err(OAuthCallbackError::AuthorizationTimeout)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn failed_authorization_setup_releases_callback_port()
    -> Result<(), Box<dyn std::error::Error>> {
        let reservation = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        let oauth = McpOAuthConfig {
            client_id: None,
            scopes: Vec::new(),
            callback_port: port,
        };

        let _ = timeout(
            Duration::from_millis(250),
            begin_authorization("fixture", "http://127.0.0.1:1/mcp", &oauth),
        )
        .await;

        let rebound = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await;
        assert!(rebound.is_ok());
        Ok(())
    }
}
