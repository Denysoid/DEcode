use std::{
    io::Cursor,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};

use backon::{BackoffBuilder, ExponentialBuilder};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream::BoxStream};
use reqwest::{
    Response, StatusCode, Url,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use secrecy::ExposeSecret;
use tokio_util::sync::CancellationToken;

use super::{
    bedrock::BedrockRuntimeTransport,
    compat::{encode_request, parse_provider_stream},
    types::{CompletedResponse, ResponsesRequest, StreamEvent, validate_completed_status},
    websocket::ResponsesWebSocketTransport,
};
use crate::{
    config::{ApiAuth, ApiCapabilities, ApiConfig, ApiProvider, ApiTransport},
    error::ApiError,
    redaction::redact_secret_values,
};

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const TOKEN_COUNT_UNKNOWN: u8 = 0;
const TOKEN_COUNT_AVAILABLE: u8 = 1;
const TOKEN_COUNT_UNAVAILABLE: u8 = 2;
const NON_PDF_FILE_RESERVE_TOKENS: u64 = 4_096;

#[derive(Clone)]
pub struct ResponsesClient {
    http: reqwest::Client,
    config: ApiConfig,
    responses_url: Url,
    bedrock_runtime: Option<BedrockRuntimeTransport>,
    websocket: Option<ResponsesWebSocketTransport>,
    token_count_support: Arc<AtomicU8>,
}

impl ResponsesClient {
    pub fn new(config: ApiConfig) -> Result<Self, ApiError> {
        config
            .validate()
            .map_err(|error| ApiError::Protocol(format!("invalid API configuration: {error}")))?;
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        let mut credential = match config.auth {
            ApiAuth::ApiKey | ApiAuth::AnthropicKey | ApiAuth::GoogleKey => {
                HeaderValue::from_str(config.api_key.expose_secret())
            }
            ApiAuth::Bearer => {
                HeaderValue::from_str(&format!("Bearer {}", config.api_key.expose_secret()))
            }
            ApiAuth::AwsSdk => Ok(HeaderValue::from_static("")),
        }
        .map_err(|error| ApiError::InvalidHeader(error.to_string()))?;
        credential.set_sensitive(true);
        match config.auth {
            ApiAuth::ApiKey => {
                headers.insert("api-key", credential);
            }
            ApiAuth::Bearer => {
                headers.insert(AUTHORIZATION, credential);
            }
            ApiAuth::AnthropicKey => {
                headers.insert("x-api-key", credential);
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
            ApiAuth::GoogleKey => {
                headers.insert("x-goog-api-key", credential);
            }
            ApiAuth::AwsSdk => {}
        }

        let responses_url = build_responses_url(&config)?;
        let websocket = (config.transport.resolved(config.provider) == ApiTransport::WebSocket)
            .then(|| ResponsesWebSocketTransport::new(&config, &responses_url))
            .transpose()?;
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .use_rustls_tls()
            .build()
            .map_err(ApiError::Transport)?;

        Ok(Self {
            http,
            bedrock_runtime: (config.provider == ApiProvider::AwsBedrockRuntime)
                .then(|| BedrockRuntimeTransport::new(config.clone())),
            websocket,
            config,
            responses_url,
            token_count_support: Arc::new(AtomicU8::new(TOKEN_COUNT_UNKNOWN)),
        })
    }

    pub fn responses_url(&self) -> &Url {
        &self.responses_url
    }

    pub fn max_attempts(&self) -> u32 {
        self.config.max_attempts
    }

    #[must_use]
    pub const fn transport(&self) -> ApiTransport {
        self.config.transport.resolved(self.config.provider)
    }

    #[must_use]
    pub const fn capabilities(&self) -> ApiCapabilities {
        self.config.provider.capabilities()
    }

    pub async fn count_input_tokens(
        &self,
        request: &ResponsesRequest,
        cancel: &CancellationToken,
    ) -> Result<Option<u64>, ApiError> {
        if !matches!(
            self.config.provider,
            ApiProvider::Azure | ApiProvider::OpenAi
        ) || self.token_count_support.load(Ordering::Relaxed) == TOKEN_COUNT_UNAVAILABLE
        {
            return Ok(None);
        }
        let Some(url) = input_tokens_url(&self.responses_url) else {
            self.token_count_support
                .store(TOKEN_COUNT_UNAVAILABLE, Ordering::Relaxed);
            return Ok(None);
        };
        if cancel.is_cancelled() {
            return Err(ApiError::Cancelled);
        }

        let mut body = serde_json::to_value(request).map_err(|error| {
            ApiError::Protocol(format!("token-count request serialization failed: {error}"))
        })?;
        let object = body.as_object_mut().ok_or_else(|| {
            ApiError::Protocol("token-count request was not an object".to_owned())
        })?;
        for field in [
            "context_management",
            "include",
            "max_output_tokens",
            "store",
            "stream",
            "temperature",
        ] {
            object.remove(field);
        }
        // The counter currently accepts only PDF file inputs, so reserve other files locally.
        let local_file_tokens = object
            .get_mut("input")
            .map(prepare_token_count_input)
            .transpose()?
            .unwrap_or(0);

        let send = self
            .http
            .post(url)
            .header(ACCEPT, "application/json")
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.config.request_timeout, send) => {
                match result {
                    Ok(result) => result.map_err(ApiError::Transport)?,
                    Err(_) => return Err(ApiError::RequestTimeout {
                        secs: self.config.request_timeout.as_secs(),
                    }),
                }
            }
        };
        let status = response.status();
        if matches!(
            status,
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED
        ) {
            self.token_count_support
                .store(TOKEN_COUNT_UNAVAILABLE, Ordering::Relaxed);
            return Ok(None);
        }
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(ApiError::Http {
                status: status.as_u16(),
                body: "authentication/authorization failed; response body omitted".to_owned(),
                retry_after_secs: None,
            });
        }
        if !status.is_success() {
            let retry_after_secs = retry_after(&response, Utc::now())
                .unwrap_or_default()
                .min(self.config.retry_after_cap.as_secs());
            let body = redact_secret_values(
                bounded_error_body(response, cancel, self.config.stream_idle_timeout).await?,
                [self.config.api_key.expose_secret()],
            );
            if token_counter_rejects_model(status, &body) {
                self.token_count_support
                    .store(TOKEN_COUNT_UNAVAILABLE, Ordering::Relaxed);
                return Ok(None);
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                return Err(ApiError::RateLimited {
                    retry_after_secs,
                    body,
                });
            }
            return Err(ApiError::Http {
                status: status.as_u16(),
                body,
                retry_after_secs: (retry_after_secs > 0).then_some(retry_after_secs),
            });
        }

        let body = bounded_error_body(response, cancel, self.config.stream_idle_timeout).await?;
        let parsed: serde_json::Value = serde_json::from_str(&body).map_err(|error| {
            ApiError::Protocol(format!("invalid token-count response: {error}"))
        })?;
        let input_tokens = parsed
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ApiError::Protocol("token-count response omitted input_tokens".to_owned())
            })?;
        self.token_count_support
            .store(TOKEN_COUNT_AVAILABLE, Ordering::Relaxed);
        Ok(Some(input_tokens.saturating_add(local_file_tokens)))
    }

    /// Execute exactly one HTTP/SSE attempt. This performs cancellation-aware
    /// request timeout handling, status/body handling, MIME validation and an
    /// idle-guarded body stream, but deliberately performs no retry.
    #[tracing::instrument(
        name = "provider.stream_attempt",
        level = "debug",
        skip_all,
        fields(
            provider = ?self.config.provider,
            model = %self.config.deployment,
            transport = ?self.transport(),
            status = "active"
        )
    )]
    pub async fn stream_response_attempt(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        if let Some(websocket) = &self.websocket {
            return websocket.stream_response_attempt(request, cancel).await;
        }
        if let Some(bedrock) = &self.bedrock_runtime {
            return bedrock.stream_response_attempt(request, cancel).await;
        }
        let response = self.send_once(&request, &cancel).await?;
        let bytes = guarded_body_stream(response, cancel, self.config.stream_idle_timeout);
        Ok(parse_provider_stream(
            bytes,
            self.config.provider.wire_protocol(),
        ))
    }

    /// Open a live event stream. Request/header failures are retried. Consumers
    /// must still require a `response.completed` event before committing or
    /// parsing assistant output.
    #[tracing::instrument(
        name = "provider.stream",
        level = "info",
        skip_all,
        fields(
            provider = ?self.config.provider,
            model = %self.config.deployment,
            transport = ?self.transport(),
            status = "active"
        )
    )]
    pub async fn stream_response(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        if let Some(websocket) = &self.websocket {
            return websocket.stream_response_attempt(request, cancel).await;
        }
        if let Some(bedrock) = &self.bedrock_runtime {
            return bedrock.stream_response_attempt(request, cancel).await;
        }
        let response = self.open_with_retry(&request, &cancel).await?;
        let bytes = guarded_body_stream(response, cancel, self.config.stream_idle_timeout);
        Ok(parse_provider_stream(
            bytes,
            self.config.provider.wire_protocol(),
        ))
    }

    /// Buffer one logical attempt until a confirmed `response.completed`.
    ///
    /// A transport/idle failure while reading the body retries the entire
    /// request and discards every event collected for the failed attempt.
    #[tracing::instrument(
        name = "provider.completed_response",
        level = "info",
        skip_all,
        fields(
            provider = ?self.config.provider,
            model = %self.config.deployment,
            transport = ?self.transport(),
            status = "active"
        )
    )]
    pub async fn completed_response(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<CompletedResponse, ApiError> {
        if self.bedrock_runtime.is_some() || self.websocket.is_some() {
            return self
                .completed_stream_transport_response(request, cancel)
                .await;
        }
        let mut attempt = 1;
        loop {
            tracing::debug!(attempt, status = "requesting", "provider request attempt");
            let response = match self.send_once(&request, &cancel).await {
                Ok(response) => response,
                Err(error) if self.is_retryable(&error) && attempt < self.config.max_attempts => {
                    self.wait_before_retry(&error, attempt, &cancel).await?;
                    attempt += 1;
                    continue;
                }
                Err(error) if self.is_retryable(&error) => {
                    return Err(ApiError::RetryExhausted {
                        attempts: attempt,
                        last_error: error.to_string(),
                    });
                }
                Err(error) => return Err(error),
            };

            let body =
                guarded_body_stream(response, cancel.clone(), self.config.stream_idle_timeout);
            let mut stream = parse_provider_stream(body, self.config.provider.wire_protocol());
            let mut events = Vec::new();
            let mut delta_text = String::new();
            let mut done_text: Option<String> = None;
            let mut retry_error = None;

            while let Some(item) = stream.next().await {
                let event = match item {
                    Ok(event) => event,
                    Err(error)
                        if matches!(
                            error,
                            ApiError::Transport(_)
                                | ApiError::IdleTimeout { .. }
                                | ApiError::RequestTimeout { .. }
                        ) && attempt < self.config.max_attempts =>
                    {
                        retry_error = Some(error);
                        break;
                    }
                    Err(error)
                        if matches!(
                            error,
                            ApiError::Transport(_)
                                | ApiError::IdleTimeout { .. }
                                | ApiError::RequestTimeout { .. }
                        ) =>
                    {
                        return Err(ApiError::RetryExhausted {
                            attempts: attempt,
                            last_error: error.to_string(),
                        });
                    }
                    Err(error) => return Err(error),
                };

                match &event {
                    StreamEvent::OutputTextDelta { delta } => delta_text.push_str(delta),
                    StreamEvent::OutputTextDone { text } => done_text = Some(text.clone()),
                    StreamEvent::Completed { response } => {
                        validate_completed_status(response)?;
                        let response = response.clone();
                        events.push(event);
                        let nested_text = response.output_text();
                        let text = if !nested_text.is_empty() {
                            nested_text
                        } else if let Some(done_text) = done_text {
                            done_text
                        } else {
                            delta_text
                        };
                        return Ok(CompletedResponse {
                            response,
                            text,
                            events,
                        });
                    }
                    StreamEvent::Failed { response } | StreamEvent::Incomplete { response } => {
                        let message = response
                            .error
                            .as_ref()
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| format!("response status: {:?}", response.status));
                        let code = response
                            .error
                            .as_ref()
                            .and_then(|error| error.code.as_deref());
                        let error = ApiError::remote(code, message);
                        match self.prepare_stream_retry(error, attempt) {
                            Ok(error) => {
                                retry_error = Some(error);
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    StreamEvent::Cancelled { response } => {
                        let message = response
                            .as_ref()
                            .and_then(|response| response.error.as_ref())
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| "remote response was cancelled".to_owned());
                        return Err(ApiError::remote(None, message));
                    }
                    StreamEvent::Error { code, message, .. } => {
                        let error = ApiError::remote(code.as_deref(), message.clone());
                        match self.prepare_stream_retry(error, attempt) {
                            Ok(error) => {
                                retry_error = Some(error);
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    StreamEvent::Created { .. } | StreamEvent::Done | StreamEvent::Ignored => {}
                }
                events.push(event);
            }

            if let Some(error) = retry_error {
                self.wait_before_retry(&error, attempt, &cancel).await?;
                attempt += 1;
                continue;
            }
            return Err(ApiError::Protocol(
                "SSE ended without response.completed".to_owned(),
            ));
        }
    }

    async fn completed_stream_transport_response(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<CompletedResponse, ApiError> {
        let mut attempt = 1;
        loop {
            tracing::debug!(attempt, status = "requesting", "stream transport attempt");
            let mut stream = match self
                .stream_response_attempt(request.clone(), cancel.child_token())
                .await
            {
                Ok(stream) => stream,
                Err(error) if self.is_retryable(&error) && attempt < self.config.max_attempts => {
                    self.wait_before_retry(&error, attempt, &cancel).await?;
                    attempt += 1;
                    continue;
                }
                Err(error) if self.is_retryable(&error) => {
                    return Err(ApiError::RetryExhausted {
                        attempts: attempt,
                        last_error: error.to_string(),
                    });
                }
                Err(error) => return Err(error),
            };
            let mut events = Vec::new();
            let mut delta_text = String::new();
            let mut done_text = None;
            let mut retry_error = None;

            while let Some(item) = stream.next().await {
                let event = match item {
                    Ok(event) => event,
                    Err(error)
                        if self.is_retryable(&error) && attempt < self.config.max_attempts =>
                    {
                        retry_error = Some(error);
                        break;
                    }
                    Err(error) if self.is_retryable(&error) => {
                        return Err(ApiError::RetryExhausted {
                            attempts: attempt,
                            last_error: error.to_string(),
                        });
                    }
                    Err(error) => return Err(error),
                };
                match &event {
                    StreamEvent::OutputTextDelta { delta } => delta_text.push_str(delta),
                    StreamEvent::OutputTextDone { text } => done_text = Some(text.clone()),
                    StreamEvent::Completed { response } => {
                        validate_completed_status(response)?;
                        let response = response.clone();
                        events.push(event);
                        let nested_text = response.output_text();
                        let text = if !nested_text.is_empty() {
                            nested_text
                        } else if let Some(done_text) = done_text {
                            done_text
                        } else {
                            delta_text
                        };
                        return Ok(CompletedResponse {
                            response,
                            text,
                            events,
                        });
                    }
                    StreamEvent::Failed { response } | StreamEvent::Incomplete { response } => {
                        let message = response
                            .error
                            .as_ref()
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| format!("response status: {:?}", response.status));
                        let code = response
                            .error
                            .as_ref()
                            .and_then(|error| error.code.as_deref());
                        let error = ApiError::remote(code, message);
                        match self.prepare_stream_retry(error, attempt) {
                            Ok(error) => {
                                retry_error = Some(error);
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    StreamEvent::Cancelled { response } => {
                        let message = response
                            .as_ref()
                            .and_then(|response| response.error.as_ref())
                            .map(|error| error.message.clone())
                            .unwrap_or_else(|| "remote response was cancelled".to_owned());
                        return Err(ApiError::remote(None, message));
                    }
                    StreamEvent::Error { code, message, .. } => {
                        let error = ApiError::remote(code.as_deref(), message.clone());
                        match self.prepare_stream_retry(error, attempt) {
                            Ok(error) => {
                                retry_error = Some(error);
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    StreamEvent::Created { .. } | StreamEvent::Done | StreamEvent::Ignored => {}
                }
                events.push(event);
            }

            if let Some(error) = retry_error {
                self.wait_before_retry(&error, attempt, &cancel).await?;
                attempt += 1;
                continue;
            }
            return Err(ApiError::Protocol(
                "stream transport ended without response.completed".to_owned(),
            ));
        }
    }

    async fn open_with_retry(
        &self,
        request: &ResponsesRequest,
        cancel: &CancellationToken,
    ) -> Result<Response, ApiError> {
        let mut attempt = 1;
        loop {
            tracing::debug!(attempt, status = "requesting", "provider open attempt");
            match self.send_once(request, cancel).await {
                Ok(response) => return Ok(response),
                Err(error) if self.is_retryable(&error) && attempt < self.config.max_attempts => {
                    self.wait_before_retry(&error, attempt, cancel).await?;
                    attempt += 1;
                }
                Err(error) if self.is_retryable(&error) => {
                    return Err(ApiError::RetryExhausted {
                        attempts: attempt,
                        last_error: error.to_string(),
                    });
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn send_once(
        &self,
        request: &ResponsesRequest,
        cancel: &CancellationToken,
    ) -> Result<Response, ApiError> {
        if cancel.is_cancelled() {
            return Err(ApiError::Cancelled);
        }
        let body = encode_request(request, self.config.provider.wire_protocol())?;
        let send = self
            .http
            .post(self.responses_url.clone())
            .json(&body)
            .send();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.config.request_timeout, send) => {
                match result {
                    Ok(result) => result.map_err(ApiError::Transport)?,
                    Err(_) => return Err(ApiError::RequestTimeout {
                        secs: self.config.request_timeout.as_secs(),
                    }),
                }
            }
        };

        let status = response.status();
        if status.is_success() {
            validate_event_stream_content_type(&response)?;
            return Ok(response);
        }

        // Never let a stalled or malformed authentication error body turn a
        // permanent 401/403 into the retryable IdleTimeout variant. Dropping
        // the response closes its body stream without following redirects or
        // exposing the credential to another request.
        if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(ApiError::Http {
                status: status.as_u16(),
                body: "authentication/authorization failed; response body omitted".to_owned(),
                retry_after_secs: None,
            });
        }

        let retry_after_secs = retry_after(&response, Utc::now())
            .unwrap_or_default()
            .min(self.config.retry_after_cap.as_secs());
        let body = redact_secret_values(
            bounded_error_body(response, cancel, self.config.stream_idle_timeout).await?,
            [self.config.api_key.expose_secret()],
        );
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(ApiError::RateLimited {
                retry_after_secs,
                body,
            });
        }
        Err(ApiError::Http {
            status: status.as_u16(),
            body,
            retry_after_secs: (retry_after_secs > 0).then_some(retry_after_secs),
        })
    }

    pub fn is_retryable(&self, error: &ApiError) -> bool {
        match error {
            ApiError::Transport(_)
            | ApiError::RequestTimeout { .. }
            | ApiError::IdleTimeout { .. }
            | ApiError::RateLimited { .. } => true,
            ApiError::Bedrock { retryable, .. } => *retryable,
            ApiError::WebSocket { retryable, .. } => *retryable,
            // Authentication/authorization failures are permanent even if a
            // proxy happens to report a non-standard status family later.
            ApiError::Http {
                status: 401 | 403, ..
            } => false,
            ApiError::Http { status, .. } => (500..=599).contains(status),
            ApiError::Remote { code, .. } => matches!(
                code.as_str(),
                " (no_capacity)"
                    | " (rate_limit_exceeded)"
                    | " (server_error)"
                    | " (internal_error)"
                    | " (service_unavailable)"
                    | " (temporarily_unavailable)"
                    | " (overloaded)"
                    | " (timeout)"
            ),
            _ => false,
        }
    }

    fn prepare_stream_retry(&self, error: ApiError, attempt: u32) -> Result<ApiError, ApiError> {
        if !self.is_retryable(&error) {
            return Err(error);
        }
        if attempt >= self.config.max_attempts {
            return Err(ApiError::RetryExhausted {
                attempts: attempt,
                last_error: error.to_string(),
            });
        }
        Ok(error)
    }

    pub async fn wait_before_retry(
        &self,
        error: &ApiError,
        attempt: u32,
        cancel: &CancellationToken,
    ) -> Result<(), ApiError> {
        let delay = match error {
            ApiError::RateLimited {
                retry_after_secs, ..
            } if *retry_after_secs > 0 => {
                Duration::from_secs(*retry_after_secs).min(self.config.retry_after_cap)
            }
            ApiError::Http {
                status: 500..=599,
                retry_after_secs: Some(retry_after_secs),
                ..
            } if *retry_after_secs > 0 => {
                Duration::from_secs(*retry_after_secs).min(self.config.retry_after_cap)
            }
            _ => jittered_backoff(
                self.config.retry_min_delay,
                self.config.retry_max_delay,
                attempt,
            ),
        };
        if delay.is_zero() {
            return Ok(());
        }
        tokio::select! {
            _ = cancel.cancelled() => Err(ApiError::Cancelled),
            _ = tokio::time::sleep(delay) => Ok(()),
        }
    }
}

fn build_responses_url(config: &ApiConfig) -> Result<Url, ApiError> {
    let resolved = config
        .endpoint
        .resolved_url(config.allow_insecure_loopback)
        .map_err(|error| ApiError::InvalidUrl(error.to_string()))?;
    let mut url = Url::parse(&resolved).map_err(|error| ApiError::InvalidUrl(error.to_string()))?;
    if config.provider == ApiProvider::Google {
        if !url.path().ends_with(":streamGenerateContent") {
            if !url.path().trim_end_matches('/').ends_with("/models") {
                return Err(ApiError::InvalidUrl(
                    "Google endpoint must end in /models or :streamGenerateContent".to_owned(),
                ));
            }
            url.path_segments_mut()
                .map_err(|()| {
                    ApiError::InvalidUrl("Google endpoint cannot be a base URL".to_owned())
                })?
                .pop_if_empty()
                .push(&format!("{}:streamGenerateContent", config.deployment));
        }
        let has_alt = url.query_pairs().any(|(name, _)| name == "alt");
        if !has_alt {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
    }
    if config.provider == ApiProvider::Azure
        && let Some(api_version) = config
            .api_version
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    {
        let retained: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(name, _)| name != "api-version")
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        let mut query = url.query_pairs_mut();
        for (name, value) in retained {
            query.append_pair(&name, &value);
        }
        query.append_pair("api-version", api_version);
    }
    Ok(url)
}

fn input_tokens_url(responses_url: &Url) -> Option<Url> {
    if !responses_url
        .path()
        .trim_end_matches('/')
        .ends_with("/responses")
    {
        return None;
    }
    let mut url = responses_url.clone();
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .push("input_tokens");
    Some(url)
}

fn token_counter_rejects_model(status: StatusCode, body: &str) -> bool {
    if status != StatusCode::BAD_REQUEST {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let Some(message) = value
        .pointer("/error/message")
        .and_then(|value| value.as_str())
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("model")
        && message.contains("not supported")
        && message.contains("responses api")
}

fn prepare_token_count_input(value: &mut serde_json::Value) -> Result<u64, ApiError> {
    if let Some((filename, file_data)) = non_pdf_inline_file(value) {
        let bytes = decode_inline_file(file_data)?;
        let tokens = estimate_non_pdf_file_tokens(filename, &bytes);
        *value = serde_json::json!({
            "type": "input_text",
            "text": "[Non-PDF attachment counted locally]",
        });
        return Ok(tokens);
    }

    match value {
        serde_json::Value::Array(items) => items.iter_mut().try_fold(0_u64, |total, item| {
            prepare_token_count_input(item).map(|tokens| total.saturating_add(tokens))
        }),
        serde_json::Value::Object(object) => object.values_mut().try_fold(0_u64, |total, item| {
            prepare_token_count_input(item).map(|tokens| total.saturating_add(tokens))
        }),
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => Ok(0),
    }
}

fn non_pdf_inline_file(value: &serde_json::Value) -> Option<(&str, &str)> {
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "input_file" {
        return None;
    }
    let filename = object.get("filename")?.as_str()?;
    let file_data = object.get("file_data")?.as_str()?;
    let mime = file_data.strip_prefix("data:")?.split_once(';')?.0;
    let is_pdf = mime.eq_ignore_ascii_case("application/pdf")
        || filename
            .rsplit_once('.')
            .is_some_and(|(_, extension)| extension.eq_ignore_ascii_case("pdf"));
    (!is_pdf).then_some((filename, file_data))
}

fn decode_inline_file(file_data: &str) -> Result<Vec<u8>, ApiError> {
    let (_, encoded) = file_data.split_once(";base64,").ok_or_else(|| {
        ApiError::Protocol("non-PDF attachment is not an inline base64 file".to_owned())
    })?;
    STANDARD.decode(encoded).map_err(|_| {
        ApiError::Protocol("non-PDF attachment contains invalid base64 data".to_owned())
    })
}

fn estimate_non_pdf_file_tokens(filename: &str, bytes: &[u8]) -> u64 {
    let raw_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let archive_text_tokens = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .filter(|extension| matches!(extension.as_str(), "docx" | "odt" | "pptx" | "xlsx"))
        .and_then(|_| archive_text_bytes(bytes))
        .map(|size| size.saturating_add(3) / 4)
        .unwrap_or(0);
    raw_bytes
        .max(archive_text_tokens)
        .saturating_add(NON_PDF_FILE_RESERVE_TOKENS)
}

fn archive_text_bytes(bytes: &[u8]) -> Option<u64> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let mut total = 0_u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index).ok()?;
        let name = file.name().to_ascii_lowercase();
        if name.ends_with(".xml")
            || name.ends_with(".rels")
            || name.ends_with(".txt")
            || name.ends_with(".csv")
        {
            total = total.saturating_add(file.size());
        }
    }
    Some(total)
}

fn validate_event_stream_content_type(response: &Response) -> Result<(), ApiError> {
    let found = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<missing>");
    let essence = found.split(';').next().unwrap_or_default().trim();
    if !essence.eq_ignore_ascii_case("text/event-stream") {
        return Err(ApiError::InvalidContentType {
            found: found.to_owned(),
        });
    }
    Ok(())
}

fn retry_after(response: &Response, now: DateTime<Utc>) -> Option<u64> {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_retry_after_value(value, now))
}

fn parse_retry_after_value(value: &str, now: DateTime<Utc>) -> Option<u64> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(seconds);
    }

    // `httpdate` accepts IMF-fixdate and the two obsolete HTTP-date forms
    // that RFC 9110 asks recipients to tolerate.
    let parsed = DateTime::<Utc>::from(httpdate::parse_http_date(value).ok()?);
    let milliseconds = parsed.signed_duration_since(now).num_milliseconds();
    if milliseconds <= 0 {
        return Some(0);
    }
    u64::try_from(milliseconds)
        .ok()
        .map(|milliseconds| milliseconds.saturating_add(999) / 1_000)
}

async fn bounded_error_body(
    response: Response,
    cancel: &CancellationToken,
    idle_timeout: Duration,
) -> Result<String, ApiError> {
    let body = response.bytes_stream();
    futures_util::pin_mut!(body);
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let item = tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(idle_timeout, body.next()) => match result {
                Ok(item) => item,
                Err(_) => return Err(ApiError::IdleTimeout { secs: idle_timeout.as_secs() }),
            }
        };
        let Some(item) = item else { break };
        let chunk = item.map_err(ApiError::Transport)?;
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(bytes.len());
        if chunk.len() > remaining {
            bytes.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk);
        if bytes.len() == MAX_ERROR_BODY_BYTES {
            truncated = true;
            break;
        }
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        text.push_str("…[truncated]");
    }
    Ok(text)
}

fn guarded_body_stream(
    response: Response,
    cancel: CancellationToken,
    idle_timeout: Duration,
) -> BoxStream<'static, Result<Bytes, ApiError>> {
    Box::pin(async_stream::stream! {
        let body = response.bytes_stream();
        futures_util::pin_mut!(body);
        loop {
            let next = tokio::time::timeout(idle_timeout, body.next());
            let item = tokio::select! {
                result = next => match result {
                    Ok(item) => item,
                    Err(_) => {
                        yield Err(ApiError::IdleTimeout { secs: idle_timeout.as_secs() });
                        return;
                    }
                },
                _ = cancel.cancelled() => {
                    yield Err(ApiError::Cancelled);
                    return;
                }
            };
            match item {
                Some(Ok(chunk)) => yield Ok(chunk),
                Some(Err(error)) => {
                    yield Err(ApiError::Transport(error));
                    return;
                }
                None => return,
            }
        }
    })
}

fn jittered_backoff(minimum: Duration, maximum: Duration, attempt: u32) -> Duration {
    if minimum.is_zero() || maximum.is_zero() || attempt == 0 {
        return Duration::ZERO;
    }
    // Backon's jitter is additive in [0, current). Starting at half our
    // configured minimum therefore preserves the previous full-jitter window
    // [ceiling/2, ceiling), while the builder owns saturation and randomness.
    let half = |duration: Duration| {
        Duration::from_millis(duration.as_millis().div_ceil(2).min(u64::MAX as u128) as u64)
    };
    ExponentialBuilder::default()
        .with_min_delay(half(minimum))
        .with_max_delay(half(maximum))
        .with_max_times(attempt as usize)
        .with_jitter()
        .build()
        .nth(attempt.saturating_sub(1) as usize)
        .unwrap_or(maximum)
        .min(maximum)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::{jittered_backoff, parse_retry_after_value};
    use crate::redaction::redact_secret_values;

    #[test]
    fn backon_retry_delay_is_bounded_and_nonzero() {
        for attempt in 1..=12 {
            let delay = jittered_backoff(
                std::time::Duration::from_millis(100),
                std::time::Duration::from_secs(2),
                attempt,
            );
            assert!(delay >= std::time::Duration::from_millis(50));
            assert!(delay <= std::time::Duration::from_secs(2));
        }
    }

    #[test]
    fn retry_after_accepts_seconds_and_http_dates() -> Result<(), &'static str> {
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 8, 13, 18, 0, 0)
            .single()
            .ok_or("invalid fixed test date")?;
        assert_eq!(parse_retry_after_value("17", now), Some(17));
        assert_eq!(
            parse_retry_after_value("Thu, 13 Aug 2026 18:00:09 GMT", now),
            Some(9)
        );
        assert_eq!(
            parse_retry_after_value("Thursday, 13-Aug-26 18:00:09 GMT", now),
            Some(9)
        );
        assert_eq!(
            parse_retry_after_value("Thu Aug 13 18:00:09 2026", now),
            Some(9)
        );
        Ok(())
    }

    #[test]
    fn provider_error_body_redacts_configured_secret() {
        let secret = "azure-test-secret-42";
        let body = redact_secret_values(
            format!("proxy echoed api-key={secret}; bearer={secret}"),
            [secret],
        );
        assert_eq!(body, "proxy echoed api-key=[REDACTED]; bearer=[REDACTED]");
        assert!(!body.contains(secret));
    }
}
