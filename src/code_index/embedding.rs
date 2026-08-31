use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use reqwest::{Client, StatusCode, Url, header};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use crate::{
    config::{ApiAuth, ApiProvider},
    error::ConfigError,
};

use super::{CodeIndexError, invalid_config};

const VECTOR_SCHEMA_VERSION: u32 = 1;
const MAX_EMBEDDING_DIMENSIONS: usize = 16_384;
const MAX_EMBEDDING_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
const MAX_VECTOR_CACHE_BYTES: u64 = 1024 * 1024 * 1024;

/// Trusted, explicit configuration for remote embeddings. It is disabled by
/// default because enabling it sends privacy-filtered repository chunks to the
/// selected provider. The API secret is always redacted from Debug output.
#[derive(Clone)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub provider: ApiProvider,
    pub auth: ApiAuth,
    pub api_key: SecretString,
    pub api_version: Option<String>,
    pub dimensions: Option<usize>,
    pub batch_size: usize,
    pub max_chunks: usize,
    pub max_input_bytes: usize,
    pub request_timeout: Duration,
    pub max_attempts: u32,
    pub hybrid_weight: f32,
}

impl std::fmt::Debug for EmbeddingConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EmbeddingConfig")
            .field("enabled", &self.enabled)
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("auth", &self.auth)
            .field("api_key", &"[REDACTED]")
            .field("api_version", &self.api_version)
            .field("dimensions", &self.dimensions)
            .field("batch_size", &self.batch_size)
            .field("max_chunks", &self.max_chunks)
            .field("max_input_bytes", &self.max_input_bytes)
            .field("request_timeout", &self.request_timeout)
            .field("max_attempts", &self.max_attempts)
            .field("hybrid_weight", &self.hybrid_weight)
            .finish()
    }
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            model: String::new(),
            provider: ApiProvider::Azure,
            auth: ApiAuth::ApiKey,
            api_key: SecretString::from(String::new()),
            api_version: None,
            dimensions: None,
            batch_size: 32,
            max_chunks: 10_000,
            max_input_bytes: 32 * 1024,
            request_timeout: Duration::from_secs(60),
            max_attempts: 3,
            hybrid_weight: 0.65,
        }
    }
}

impl EmbeddingConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.enabled {
            return Ok(());
        }
        if self.endpoint.trim().is_empty() || self.model.trim().is_empty() {
            return Err(invalid_config(
                "code_index.embeddings",
                "endpoint and model are required when remote embeddings are enabled",
            ));
        }
        let url = Url::parse(&self.endpoint).map_err(|error| {
            invalid_config(
                "code_index.embeddings.endpoint",
                format!("must be an absolute HTTP(S) URL: {error}"),
            )
        })?;
        if url.cannot_be_a_base()
            || url.host_str().is_none()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid_config(
                "code_index.embeddings.endpoint",
                "must be an absolute URL without credentials or a fragment",
            ));
        }
        let loopback = url.host_str().is_some_and(|host| {
            let host = host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                .unwrap_or(host);
            host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
                || host
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(invalid_config(
                "code_index.embeddings.endpoint",
                "must use HTTPS (plaintext HTTP is accepted only for loopback testing)",
            ));
        }
        if self.api_key.expose_secret().is_empty() {
            return Err(invalid_config(
                "code_index.embeddings",
                "the selected provider API key is unavailable",
            ));
        }
        if self.provider == ApiProvider::Azure && self.auth != ApiAuth::ApiKey {
            return Err(invalid_config(
                "code_index.embeddings.auth",
                "Azure embeddings require api_key authentication",
            ));
        }
        if self.provider == ApiProvider::OpenAi && self.auth != ApiAuth::Bearer {
            return Err(invalid_config(
                "code_index.embeddings.auth",
                "OpenAI embeddings require bearer authentication",
            ));
        }
        if self.provider != ApiProvider::Azure && self.api_version.is_some() {
            return Err(invalid_config(
                "code_index.embeddings.api_version",
                "api_version is Azure-only",
            ));
        }
        if self
            .dimensions
            .is_some_and(|value| value == 0 || value > MAX_EMBEDDING_DIMENSIONS)
        {
            return Err(invalid_config(
                "code_index.embeddings.dimensions",
                format!("must be between 1 and {MAX_EMBEDDING_DIMENSIONS}"),
            ));
        }
        if !(1..=128).contains(&self.batch_size) {
            return Err(invalid_config(
                "code_index.embeddings.batch_size",
                "must be between 1 and 128",
            ));
        }
        if !(1..=100_000).contains(&self.max_chunks) {
            return Err(invalid_config(
                "code_index.embeddings.max_chunks",
                "must be between 1 and 100000",
            ));
        }
        if !(1_024..=256 * 1024).contains(&self.max_input_bytes) {
            return Err(invalid_config(
                "code_index.embeddings.max_input_bytes",
                "must be between 1024 and 262144",
            ));
        }
        if self.request_timeout.is_zero() || self.request_timeout > Duration::from_secs(300) {
            return Err(invalid_config(
                "code_index.embeddings.request_timeout_secs",
                "must be between 1 and 300 seconds",
            ));
        }
        if !(1..=5).contains(&self.max_attempts) {
            return Err(invalid_config(
                "code_index.embeddings.max_attempts",
                "must be between 1 and 5",
            ));
        }
        if !self.hybrid_weight.is_finite() || !(0.0..=1.0).contains(&self.hybrid_weight) {
            return Err(invalid_config(
                "code_index.embeddings.hybrid_weight",
                "must be a finite number between 0 and 1",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.endpoint.as_bytes());
        hasher.update([0]);
        hasher.update(self.model.as_bytes());
        hasher.update([0]);
        hasher.update(self.provider.label().as_bytes());
        hasher.update([0]);
        hasher.update(format!("{:?}", self.auth).as_bytes());
        hasher.update([0]);
        hasher.update(self.api_version.as_deref().unwrap_or_default().as_bytes());
        hasher.update([0]);
        hasher.update(self.dimensions.unwrap_or_default().to_le_bytes());
        hasher.update(self.max_chunks.to_le_bytes());
        hasher.update(self.max_input_bytes.to_le_bytes());
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Clone, Debug)]
pub(super) struct EmbeddingChunk {
    pub key: String,
    pub input: String,
}

#[derive(Clone, Debug)]
pub(super) struct VectorIndex {
    dimensions: usize,
    records: BTreeMap<String, Arc<[f32]>>,
}

impl VectorIndex {
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn score(&self, query: &[f32], key: &str) -> Option<f64> {
        let vector = self.records.get(key)?;
        if query.len() != self.dimensions || vector.len() != self.dimensions {
            return None;
        }
        Some(
            query
                .iter()
                .zip(vector.iter())
                .map(|(left, right)| f64::from(*left) * f64::from(*right))
                .sum::<f64>()
                .clamp(-1.0, 1.0),
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedVectorIndex {
    schema_version: u32,
    workspace_fingerprint: String,
    embedding_fingerprint: String,
    dimensions: usize,
    records: BTreeMap<String, Vec<f32>>,
}

#[derive(Debug)]
pub(super) struct EmbeddingBuildOutput {
    pub index: VectorIndex,
    pub cache_bytes: u64,
    pub reused: usize,
    pub embedded: usize,
}

#[derive(Clone, Debug)]
pub(super) struct EmbeddingClient {
    config: EmbeddingConfig,
    client: Client,
    endpoint: Url,
}

impl EmbeddingClient {
    pub fn new(config: EmbeddingConfig) -> Result<Self, CodeIndexError> {
        config
            .validate()
            .map_err(|error| CodeIndexError::Embedding(error.to_string()))?;
        let mut endpoint = Url::parse(&config.endpoint)
            .map_err(|error| CodeIndexError::Embedding(error.to_string()))?;
        if config.provider == ApiProvider::Azure
            && let Some(version) = config.api_version.as_deref()
        {
            let retained = endpoint
                .query_pairs()
                .filter(|(name, _)| name != "api-version")
                .map(|(name, value)| (name.into_owned(), value.into_owned()))
                .collect::<Vec<_>>();
            endpoint.set_query(None);
            let mut query = endpoint.query_pairs_mut();
            for (name, value) in retained {
                query.append_pair(&name, &value);
            }
            query.append_pair("api-version", version);
        }
        let client = Client::builder()
            .connect_timeout(config.request_timeout.min(Duration::from_secs(30)))
            .timeout(config.request_timeout)
            .build()
            .map_err(|error| CodeIndexError::Embedding(error.to_string()))?;
        Ok(Self {
            config,
            client,
            endpoint,
        })
    }

    pub async fn embed(
        &self,
        inputs: &[String],
        cancel: &CancellationToken,
    ) -> Result<Vec<Arc<[f32]>>, CodeIndexError> {
        if inputs.is_empty() || inputs.len() > self.config.batch_size {
            return Err(CodeIndexError::Embedding(format!(
                "embedding batch must contain 1..={} inputs",
                self.config.batch_size
            )));
        }
        let body = EmbeddingRequest {
            model: &self.config.model,
            input: inputs,
            dimensions: self.config.dimensions,
            encoding_format: "float",
        };
        let mut last_error = None;
        for attempt in 1..=self.config.max_attempts {
            let mut request = self
                .client
                .post(self.endpoint.clone())
                .header(header::CONTENT_TYPE, "application/json");
            request = match self.config.auth {
                ApiAuth::ApiKey => request.header("api-key", self.config.api_key.expose_secret()),
                ApiAuth::Bearer => request.bearer_auth(self.config.api_key.expose_secret()),
                ApiAuth::AnthropicKey => {
                    request.header("x-api-key", self.config.api_key.expose_secret())
                }
                ApiAuth::GoogleKey => {
                    request.header("x-goog-api-key", self.config.api_key.expose_secret())
                }
                ApiAuth::AwsSdk => {
                    return Err(CodeIndexError::Embedding(
                        "native AWS Bedrock credentials cannot authenticate the OpenAI embeddings endpoint"
                            .to_owned(),
                    ));
                }
            };
            let response = tokio::select! {
                _ = cancel.cancelled() => return Err(CodeIndexError::Cancelled),
                response = request.json(&body).send() => response,
            };
            match response {
                Ok(response) if response.status().is_success() => {
                    let bytes = read_success_body(response, cancel).await?;
                    let decoded: EmbeddingResponse = serde_json::from_slice(&bytes)
                        .map_err(|error| CodeIndexError::Embedding(error.to_string()))?;
                    return validate_response(decoded, inputs.len(), self.config.dimensions);
                }
                Ok(response) => {
                    let status = response.status();
                    let retryable =
                        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                    let detail = bounded_error_body(response, cancel).await?;
                    let error = format!("embedding HTTP {status}: {detail}");
                    if !retryable || attempt == self.config.max_attempts {
                        return Err(CodeIndexError::Embedding(error));
                    }
                    last_error = Some(error);
                }
                Err(error) => {
                    let retryable = error.is_timeout() || error.is_connect();
                    if !retryable || attempt == self.config.max_attempts {
                        return Err(CodeIndexError::Embedding(error.to_string()));
                    }
                    last_error = Some(error.to_string());
                }
            }
            let delay = Duration::from_millis(250_u64.saturating_mul(1_u64 << attempt.min(4)));
            tokio::select! {
                _ = cancel.cancelled() => return Err(CodeIndexError::Cancelled),
                _ = tokio::time::sleep(delay) => {}
            }
        }
        Err(CodeIndexError::Embedding(
            last_error.unwrap_or_else(|| "embedding request failed".to_owned()),
        ))
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Deserialize)]
struct EmbeddingData {
    index: usize,
    embedding: Vec<f32>,
}

fn validate_response(
    response: EmbeddingResponse,
    expected: usize,
    configured_dimensions: Option<usize>,
) -> Result<Vec<Arc<[f32]>>, CodeIndexError> {
    if response.data.len() != expected {
        return Err(CodeIndexError::Embedding(format!(
            "provider returned {} vectors for {expected} inputs",
            response.data.len()
        )));
    }
    let mut ordered = vec![None; expected];
    let mut dimensions = configured_dimensions;
    for item in response.data {
        if item.index >= expected || ordered[item.index].is_some() {
            return Err(CodeIndexError::Embedding(
                "provider returned duplicate or out-of-range embedding indexes".to_owned(),
            ));
        }
        if item.embedding.is_empty()
            || item.embedding.len() > MAX_EMBEDDING_DIMENSIONS
            || item.embedding.iter().any(|value| !value.is_finite())
        {
            return Err(CodeIndexError::Embedding(
                "provider returned an empty, oversized, or non-finite vector".to_owned(),
            ));
        }
        match dimensions {
            Some(value) if value != item.embedding.len() => {
                return Err(CodeIndexError::Embedding(format!(
                    "embedding dimension mismatch: expected {value}, got {}",
                    item.embedding.len()
                )));
            }
            None => dimensions = Some(item.embedding.len()),
            _ => {}
        }
        ordered[item.index] = Some(normalize(item.embedding)?);
    }
    ordered
        .into_iter()
        .map(|item| {
            item.ok_or_else(|| CodeIndexError::Embedding("embedding index is missing".to_owned()))
        })
        .collect()
}

fn normalize(mut vector: Vec<f32>) -> Result<Arc<[f32]>, CodeIndexError> {
    let norm = vector
        .iter()
        .map(|value| f64::from(*value).powi(2))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm <= f64::EPSILON {
        return Err(CodeIndexError::Embedding(
            "provider returned a zero-length vector".to_owned(),
        ));
    }
    for value in &mut vector {
        *value = (f64::from(*value) / norm) as f32;
    }
    Ok(Arc::from(vector))
}

async fn read_success_body(
    mut response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, CodeIndexError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CodeIndexError::Cancelled),
            chunk = response.chunk() => chunk,
        }
        .map_err(|error| CodeIndexError::Embedding(error.to_string()))?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        if chunk.len() > MAX_EMBEDDING_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(CodeIndexError::Embedding(format!(
                "embedding response exceeds {MAX_EMBEDDING_RESPONSE_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
}

async fn bounded_error_body(
    mut response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<String, CodeIndexError> {
    const LIMIT: usize = 2_048;
    let mut bytes = Vec::new();
    while bytes.len() < LIMIT {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Err(CodeIndexError::Cancelled),
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(chunk)) => {
                let remaining = LIMIT.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
            }
            Ok(None) => break,
            Err(error) => return Ok(error.to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&bytes)
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect())
}

pub(super) async fn load_vector_cache(
    path: PathBuf,
    workspace_fingerprint: String,
    embedding_fingerprint: String,
    valid_keys: BTreeSet<String>,
) -> Result<Option<(VectorIndex, u64)>, CodeIndexError> {
    tokio::task::spawn_blocking(move || {
        if !path.exists() {
            return Ok(None);
        }
        let metadata = std::fs::metadata(&path).map_err(|source| CodeIndexError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.len() > MAX_VECTOR_CACHE_BYTES {
            return Err(CodeIndexError::InvalidCache {
                path,
                message: "vector cache exceeds the safety limit".to_owned(),
            });
        }
        let bytes = std::fs::read(&path).map_err(|source| CodeIndexError::Io {
            path: path.clone(),
            source,
        })?;
        let persisted: PersistedVectorIndex =
            serde_json::from_slice(&bytes).map_err(|source| CodeIndexError::Json {
                path: path.clone(),
                source,
            })?;
        if persisted.schema_version != VECTOR_SCHEMA_VERSION
            || persisted.workspace_fingerprint != workspace_fingerprint
            || persisted.embedding_fingerprint != embedding_fingerprint
            || persisted.dimensions == 0
            || persisted.dimensions > MAX_EMBEDDING_DIMENSIONS
        {
            return Ok(None);
        }
        let mut records = BTreeMap::new();
        for (key, vector) in persisted.records {
            if !valid_keys.contains(&key)
                || vector.len() != persisted.dimensions
                || vector.iter().any(|value| !value.is_finite())
            {
                continue;
            }
            if let Ok(vector) = normalize(vector) {
                records.insert(key, vector);
            }
        }
        if records.is_empty() {
            return Ok(None);
        }
        Ok(Some((
            VectorIndex {
                dimensions: persisted.dimensions,
                records,
            },
            metadata.len(),
        )))
    })
    .await
    .map_err(|error| CodeIndexError::Worker(error.to_string()))?
}

pub(super) async fn build_vector_index(
    client: EmbeddingClient,
    chunks: Vec<EmbeddingChunk>,
    previous: Option<Arc<VectorIndex>>,
    cache_path: PathBuf,
    workspace_fingerprint: String,
    embedding_fingerprint: String,
    cancel: CancellationToken,
) -> Result<EmbeddingBuildOutput, CodeIndexError> {
    let mut records = BTreeMap::new();
    let mut reused = 0_usize;
    let mut pending = Vec::new();
    for chunk in chunks {
        if let Some(vector) = previous
            .as_ref()
            .and_then(|index| index.records.get(&chunk.key))
        {
            records.insert(chunk.key, Arc::clone(vector));
            reused = reused.saturating_add(1);
        } else {
            pending.push(chunk);
        }
    }
    let embedded = pending.len();
    for batch in pending.chunks(client.config.batch_size) {
        if cancel.is_cancelled() {
            return Err(CodeIndexError::Cancelled);
        }
        let inputs = batch
            .iter()
            .map(|chunk| chunk.input.clone())
            .collect::<Vec<_>>();
        let vectors = client.embed(&inputs, &cancel).await?;
        for (chunk, vector) in batch.iter().zip(vectors) {
            records.insert(chunk.key.clone(), vector);
        }
    }
    let dimensions = records.values().next().map_or(0, |vector| vector.len());
    if dimensions == 0 {
        return Err(CodeIndexError::Embedding(
            "no repository chunks were eligible for embeddings".to_owned(),
        ));
    }
    let persisted = PersistedVectorIndex {
        schema_version: VECTOR_SCHEMA_VERSION,
        workspace_fingerprint,
        embedding_fingerprint,
        dimensions,
        records: records
            .iter()
            .map(|(key, vector)| (key.clone(), vector.to_vec()))
            .collect(),
    };
    let cache_bytes = persist_vector_cache(&cache_path, &persisted)?;
    Ok(EmbeddingBuildOutput {
        index: VectorIndex {
            dimensions,
            records,
        },
        cache_bytes,
        reused,
        embedded,
    })
}

fn persist_vector_cache(
    path: &Path,
    persisted: &PersistedVectorIndex,
) -> Result<u64, CodeIndexError> {
    let parent = path.parent().ok_or_else(|| CodeIndexError::InvalidCache {
        path: path.to_path_buf(),
        message: "vector cache has no parent directory".to_owned(),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| CodeIndexError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CodeIndexError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    serde_json::to_writer(&mut temporary, persisted).map_err(|source| CodeIndexError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    temporary.flush().map_err(|source| CodeIndexError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let length = temporary
        .as_file()
        .metadata()
        .map_err(|source| CodeIndexError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > MAX_VECTOR_CACHE_BYTES {
        return Err(CodeIndexError::InvalidCache {
            path: path.to_path_buf(),
            message: "generated vector cache exceeds the safety limit".to_owned(),
        });
    }
    temporary
        .persist(path)
        .map_err(|error| CodeIndexError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(length)
}

#[must_use]
pub(super) fn vector_key(path: &str, start_line: usize, end_line: usize, text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    hasher.update([0]);
    hasher.update(start_line.to_le_bytes());
    hasher.update(end_line.to_le_bytes());
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[must_use]
pub(super) fn bounded_embedding_input(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let suffix = "\n[chunk truncated before embedding]";
    let mut end = limit.saturating_sub(suffix.len());
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{suffix}", &value[..end])
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    use secrecy::SecretString;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    use super::{
        EmbeddingClient, EmbeddingConfig, PersistedVectorIndex, VECTOR_SCHEMA_VERSION,
        bounded_embedding_input, load_vector_cache,
    };
    use crate::config::{ApiAuth, ApiProvider};

    fn config(endpoint: String) -> EmbeddingConfig {
        EmbeddingConfig {
            enabled: true,
            endpoint,
            model: "embedding-deployment".to_owned(),
            provider: ApiProvider::Azure,
            auth: ApiAuth::ApiKey,
            api_key: SecretString::from("secret".to_owned()),
            api_version: None,
            dimensions: Some(3),
            batch_size: 8,
            max_chunks: 100,
            max_input_bytes: 4_096,
            request_timeout: Duration::from_secs(2),
            max_attempts: 1,
            hybrid_weight: 0.65,
        }
    }

    #[test]
    fn endpoint_validation_accepts_ipv6_loopback_http() {
        assert!(
            config("http://[::1]:8080/embeddings".to_owned())
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn endpoint_validation_rejects_embedded_credentials() {
        assert!(
            config("https://user:secret@example.test/embeddings".to_owned())
                .validate()
                .is_err()
        );
    }

    #[test]
    fn cache_fingerprint_tracks_vector_affecting_settings() {
        let base = config("https://example.test/embeddings".to_owned());
        let fingerprint = base.fingerprint();

        let mut changed_input = base.clone();
        changed_input.max_input_bytes += 1;
        assert_ne!(changed_input.fingerprint(), fingerprint);

        let mut changed_limit = base.clone();
        changed_limit.max_chunks += 1;
        assert_ne!(changed_limit.fingerprint(), fingerprint);

        let mut changed_version = base;
        changed_version.api_version = Some("next".to_owned());
        assert_ne!(changed_version.fingerprint(), fingerprint);
    }

    #[tokio::test]
    async fn response_is_index_ordered_and_unit_normalized()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    {"index": 1, "embedding": [0.0, 3.0, 4.0]},
                    {"index": 0, "embedding": [2.0, 0.0, 0.0]}
                ]
            })))
            .mount(&server)
            .await;
        let client = EmbeddingClient::new(config(format!("{}/embeddings", server.uri())))?;
        let vectors = client
            .embed(
                &["one".to_owned(), "two".to_owned()],
                &tokio_util::sync::CancellationToken::new(),
            )
            .await?;
        assert_eq!(&*vectors[0], &[1.0, 0.0, 0.0]);
        assert_eq!(&*vectors[1], &[0.0, 0.6, 0.8]);
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_stalled_error_body() -> Result<(), Box<dyn std::error::Error>>
    {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let server = thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(1)))?;
            let mut request = [0_u8; 2_048];
            let _ = stream.read(&mut request)?;
            stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100000\r\nConnection: close\r\n\r\npartial",
            )?;
            stream.flush()?;
            thread::sleep(Duration::from_millis(500));
            Ok(())
        });
        let client = EmbeddingClient::new(config(format!("http://{address}/embeddings")))?;
        let cancel = tokio_util::sync::CancellationToken::new();
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            cancel_task.cancel();
        });

        let started = Instant::now();
        let result = client.embed(&["one".to_owned()], &cancel).await;
        assert!(matches!(result, Err(super::CodeIndexError::Cancelled)));
        assert!(started.elapsed() < Duration::from_millis(250));
        server.join().map_err(|_| "server thread panicked")??;
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_zero_vector_does_not_discard_valid_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("vectors.json");
        let persisted = PersistedVectorIndex {
            schema_version: VECTOR_SCHEMA_VERSION,
            workspace_fingerprint: "workspace".to_owned(),
            embedding_fingerprint: "embedding".to_owned(),
            dimensions: 2,
            records: [
                ("valid".to_owned(), vec![3.0, 4.0]),
                ("zero".to_owned(), vec![0.0, 0.0]),
            ]
            .into_iter()
            .collect(),
        };
        std::fs::write(&path, serde_json::to_vec(&persisted)?)?;

        let (index, _) = load_vector_cache(
            path,
            "workspace".to_owned(),
            "embedding".to_owned(),
            ["valid".to_owned(), "zero".to_owned()]
                .into_iter()
                .collect(),
        )
        .await?
        .ok_or("cache was unexpectedly ignored")?;

        assert_eq!(index.len(), 1);
        assert!(index.score(&[0.6, 0.8], "valid").is_some());

        let empty = load_vector_cache(
            directory.path().join("vectors.json"),
            "workspace".to_owned(),
            "embedding".to_owned(),
            ["zero".to_owned()].into_iter().collect(),
        )
        .await?;
        assert!(empty.is_none());
        Ok(())
    }

    #[test]
    fn truncation_is_utf8_safe_and_explicit() {
        let input = "é".repeat(2_000);
        let bounded = bounded_embedding_input(&input, 1_025);
        assert!(bounded.len() <= 1_025);
        assert!(bounded.ends_with("[chunk truncated before embedding]"));
    }
}
