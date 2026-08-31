use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use async_stream::stream;
use futures_util::{SinkExt, StreamExt, stream::BoxStream};
use reqwest::{Url, header::HeaderValue};
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message, client::IntoClientRequest, error::Error as TungsteniteError,
        http::HeaderValue as WsHeaderValue,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{
    api::{
        stream::{MAX_SSE_TURN_BYTES, decode_responses_event},
        types::{ResponsesRequest, StreamEvent},
    },
    config::{ApiAuth, ApiConfig},
    error::ApiError,
};

static STREAM_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct ResponsesWebSocketTransport {
    url: Url,
    credential: HeaderValue,
    request_timeout: Duration,
    idle_timeout: Duration,
}

impl ResponsesWebSocketTransport {
    pub(crate) fn new(config: &ApiConfig, responses_url: &Url) -> Result<Self, ApiError> {
        if config.auth != ApiAuth::Bearer {
            return Err(ApiError::Protocol(
                "Responses WebSocket requires bearer authentication".to_owned(),
            ));
        }
        let mut url = responses_url.clone();
        let scheme = match url.scheme() {
            "https" => "wss",
            "http" if config.allow_insecure_loopback => "ws",
            other => {
                return Err(ApiError::InvalidUrl(format!(
                    "cannot derive a WebSocket endpoint from {other:?}"
                )));
            }
        };
        url.set_scheme(scheme).map_err(|_| {
            ApiError::InvalidUrl("failed to select WebSocket URL scheme".to_owned())
        })?;
        let mut credential =
            HeaderValue::from_str(&format!("Bearer {}", config.api_key.expose_secret()))
                .map_err(|error| ApiError::InvalidHeader(error.to_string()))?;
        credential.set_sensitive(true);
        Ok(Self {
            url,
            credential,
            request_timeout: config.request_timeout,
            idle_timeout: config.stream_idle_timeout,
        })
    }

    pub(crate) async fn stream_response_attempt(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        if cancel.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        let stream_id = next_stream_id();
        let create = create_event(request, &stream_id)?;
        let mut handshake = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|error| websocket_error("handshake", error))?;
        let mut credential = WsHeaderValue::from_bytes(self.credential.as_bytes())
            .map_err(|error| ApiError::InvalidHeader(error.to_string()))?;
        credential.set_sensitive(true);
        handshake.headers_mut().insert("authorization", credential);

        let connect = connect_async(handshake);
        let (mut socket, _) = tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.request_timeout, connect) => match result {
                Ok(Ok(connected)) => connected,
                Ok(Err(error)) => return Err(websocket_error("handshake", error)),
                Err(_) => return Err(ApiError::RequestTimeout { secs: self.request_timeout.as_secs() }),
            },
        };
        tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.request_timeout, socket.send(Message::Text(create.into()))) => match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => return Err(websocket_error("send", error)),
                Err(_) => return Err(ApiError::RequestTimeout { secs: self.request_timeout.as_secs() }),
            },
        }

        let idle_timeout = self.idle_timeout;
        let event_stream = stream! {
            let mut total_bytes = 0_usize;
            let mut last_sequence = None::<u64>;
            loop {
                let received = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(ApiError::Cancelled);
                        return;
                    }
                    result = tokio::time::timeout(idle_timeout, socket.next()) => result,
                };
                let message = match received {
                    Ok(Some(Ok(message))) => message,
                    Ok(Some(Err(error))) => {
                        yield Err(websocket_error("stream", error));
                        return;
                    }
                    Ok(None) => {
                        yield Err(ApiError::WebSocket {
                            stage: "stream".to_owned(),
                            message: "connection closed before response.completed".to_owned(),
                            retryable: true,
                        });
                        return;
                    }
                    Err(_) => {
                        yield Err(ApiError::IdleTimeout { secs: idle_timeout.as_secs() });
                        return;
                    }
                };
                let encoded = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Binary(bytes) => match String::from_utf8(bytes.to_vec()) {
                        Ok(text) => text,
                        Err(error) => {
                            yield Err(ApiError::Protocol(format!(
                                "Responses WebSocket sent non-UTF-8 binary data: {error}"
                            )));
                            return;
                        }
                    },
                    Message::Ping(payload) => {
                        if let Err(error) = socket.send(Message::Pong(payload)).await {
                            yield Err(websocket_error("pong", error));
                            return;
                        }
                        continue;
                    }
                    Message::Pong(_) | Message::Frame(_) => continue,
                    Message::Close(_) => {
                        yield Err(ApiError::WebSocket {
                            stage: "stream".to_owned(),
                            message: "connection closed before response.completed".to_owned(),
                            retryable: true,
                        });
                        return;
                    }
                };
                total_bytes = total_bytes.saturating_add(encoded.len());
                if total_bytes > MAX_SSE_TURN_BYTES {
                    yield Err(ApiError::Protocol(format!(
                        "Responses WebSocket turn exceeds {MAX_SSE_TURN_BYTES} bytes"
                    )));
                    return;
                }
                let value: Value = match serde_json::from_str(&encoded) {
                    Ok(value) => value,
                    Err(error) => {
                        yield Err(ApiError::Protocol(format!(
                            "malformed Responses WebSocket event: {error}"
                        )));
                        return;
                    }
                };
                if let Some(actual) = value.get("stream_id").and_then(Value::as_str)
                    && actual != stream_id
                {
                    yield Err(ApiError::Protocol(format!(
                        "Responses WebSocket event belongs to unexpected stream {actual:?}"
                    )));
                    return;
                }
                if let Some(sequence) = value.get("sequence_number").and_then(Value::as_u64) {
                    if last_sequence.is_some_and(|previous| sequence <= previous) {
                        continue;
                    }
                    last_sequence = Some(sequence);
                }
                let event = match decode_responses_event(&encoded) {
                    Ok(event) => event,
                    Err(error) => {
                        yield Err(ApiError::Protocol(format!(
                            "malformed Responses WebSocket event: {error}"
                        )));
                        return;
                    }
                };
                let terminal = matches!(
                    event,
                    StreamEvent::Completed { .. }
                        | StreamEvent::Failed { .. }
                        | StreamEvent::Incomplete { .. }
                        | StreamEvent::Cancelled { .. }
                        | StreamEvent::Error { .. }
                );
                yield Ok(event);
                if terminal {
                    let _ = socket.close(None).await;
                    return;
                }
            }
        };
        Ok(Box::pin(event_stream))
    }
}

fn create_event(request: ResponsesRequest, stream_id: &str) -> Result<String, ApiError> {
    let value = serde_json::to_value(request)
        .map_err(|error| ApiError::Protocol(format!("request serialization failed: {error}")))?;
    let Value::Object(mut object) = value else {
        return Err(ApiError::Protocol(
            "Responses WebSocket request was not an object".to_owned(),
        ));
    };
    object.remove("stream");
    object.insert(
        "type".to_owned(),
        Value::String("response.create".to_owned()),
    );
    object.insert("stream_id".to_owned(), Value::String(stream_id.to_owned()));
    serde_json::to_string(&Value::Object(object))
        .map_err(|error| ApiError::Protocol(format!("request serialization failed: {error}")))
}

fn next_stream_id() -> String {
    let sequence = STREAM_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("decode-{sequence}")
}

fn websocket_error(stage: &str, error: TungsteniteError) -> ApiError {
    let retryable = match &error {
        TungsteniteError::ConnectionClosed
        | TungsteniteError::AlreadyClosed
        | TungsteniteError::Io(_)
        | TungsteniteError::Tls(_) => true,
        TungsteniteError::Http(response) => {
            let status = response.status().as_u16();
            status == 429 || (500..=599).contains(&status)
        }
        _ => false,
    };
    ApiError::WebSocket {
        stage: stage.to_owned(),
        message: bounded_error(&error.to_string()),
        retryable,
    }
}

fn bounded_error(raw: &str) -> String {
    raw.chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(2_048)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::types::{InputMessage, ResponseStatus};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    fn request() -> ResponsesRequest {
        ResponsesRequest::new("gpt-test", "system", vec![InputMessage::user("hello")], 256)
    }

    fn local_transport(
        address: std::net::SocketAddr,
    ) -> Result<ResponsesWebSocketTransport, Box<dyn std::error::Error + Send + Sync>> {
        Ok(ResponsesWebSocketTransport {
            url: Url::parse(&format!("ws://{address}/v1/responses"))?,
            credential: HeaderValue::from_static("Bearer test-secret"),
            request_timeout: Duration::from_secs(2),
            idle_timeout: Duration::from_secs(2),
        })
    }

    #[test]
    fn create_event_uses_input_and_removes_http_stream_flag() -> Result<(), ApiError> {
        let encoded = create_event(request(), "main.1")?;
        let value: Value = serde_json::from_str(&encoded)
            .map_err(|error| ApiError::Protocol(error.to_string()))?;
        assert_eq!(value["type"], "response.create");
        assert_eq!(value["stream_id"], "main.1");
        assert_eq!(value["input"][0]["content"], "hello");
        assert!(value.get("stream").is_none());
        Ok(())
    }

    #[test]
    fn stream_ids_match_the_documented_character_and_length_contract() {
        let stream_id = next_stream_id();
        assert!(!stream_id.is_empty());
        assert!(stream_id.len() <= 256);
        assert!(
            stream_id
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || "_.-".contains(character))
        );
    }

    #[tokio::test]
    async fn websocket_stream_deduplicates_sequences_and_completes()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut socket = accept_async(tcp).await?;
            let create = socket
                .next()
                .await
                .ok_or("client closed before response.create")??;
            let create: Value = serde_json::from_str(create.to_text()?)?;
            let stream_id = create
                .get("stream_id")
                .and_then(Value::as_str)
                .ok_or("response.create omitted stream_id")?;
            assert_eq!(create["type"], "response.create");
            assert!(create.get("stream").is_none());

            for event in [
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "stream_id": stream_id,
                    "sequence_number": 1,
                    "delta": "hello"
                }),
                serde_json::json!({
                    "type": "response.output_text.delta",
                    "stream_id": stream_id,
                    "sequence_number": 1,
                    "delta": "duplicate"
                }),
                serde_json::json!({
                    "type": "response.completed",
                    "stream_id": stream_id,
                    "sequence_number": 2,
                    "response": {
                        "id": "resp_ws",
                        "status": "completed",
                        "output": [],
                        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
                    }
                }),
            ] {
                socket.send(Message::Text(event.to_string().into())).await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let transport = local_transport(address)?;
        let mut stream = transport
            .stream_response_attempt(request(), CancellationToken::new())
            .await?;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event?);
        }
        server.await??;

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            StreamEvent::OutputTextDelta {
                delta: "hello".to_owned()
            }
        );
        assert!(matches!(
            &events[1],
            StreamEvent::Completed { response }
                if response.id == "resp_ws"
                    && response.status == Some(ResponseStatus::Completed)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn premature_websocket_close_is_retryable()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await?;
            let mut socket = accept_async(tcp).await?;
            let _ = socket.next().await;
            socket.close(None).await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });

        let transport = local_transport(address)?;
        let mut stream = transport
            .stream_response_attempt(request(), CancellationToken::new())
            .await?;
        let event = stream.next().await.ok_or("missing close error")?;
        server.await??;
        assert!(matches!(
            event,
            Err(ApiError::WebSocket {
                retryable: true,
                ..
            })
        ));
        assert!(stream.next().await.is_none());
        Ok(())
    }
}
