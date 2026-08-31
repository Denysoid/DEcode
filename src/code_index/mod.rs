use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    api::FunctionToolDefinition, error::ConfigError, notice::UiNotice, privacy::PrivacyShield,
};

mod embedding;

pub use embedding::EmbeddingConfig;
use embedding::{
    EmbeddingBuildOutput, EmbeddingChunk, EmbeddingClient, VectorIndex, bounded_embedding_input,
    build_vector_index, load_vector_cache, vector_key,
};

pub const INDEX_STATUS_TOOL: &str = "code_index_status";
pub const INDEX_SEARCH_TOOL: &str = "codebase_search";
pub const INDEX_OVERVIEW_TOOL: &str = "codebase_overview";
pub const INDEX_DEPENDENCIES_TOOL: &str = "codebase_dependencies";

const SCHEMA_VERSION: u32 = 1;
const DEFAULT_MAX_FILES: usize = 10_000;
const DEFAULT_MAX_FILE_BYTES: usize = 512 * 1024;
const DEFAULT_MAX_SOURCE_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_CHUNKS: usize = 50_000;
const DEFAULT_CHUNK_LINES: usize = 100;
const DEFAULT_OVERLAP_LINES: usize = 12;
const DEFAULT_MAX_RESULT_BYTES: usize = 128 * 1024;
const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_QUERY_BYTES: usize = 4_096;
const MAX_PATH_FILTER_BYTES: usize = 16 * 1024;
const MAX_SEARCH_RESULTS: usize = 32;
const MAX_SNIPPET_BYTES: usize = 8 * 1024;
const MAX_SYMBOLS_PER_CHUNK: usize = 64;
const MAX_IMPORTS_PER_FILE: usize = 256;
const MAX_TOKEN_BYTES: usize = 128;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct CodeIndexConfig {
    pub enabled: bool,
    pub auto_refresh: bool,
    pub max_files: usize,
    pub max_file_bytes: usize,
    pub max_source_bytes: usize,
    pub max_chunks: usize,
    pub chunk_lines: usize,
    pub overlap_lines: usize,
    pub max_result_bytes: usize,
    pub embeddings: EmbeddingConfig,
}

impl Default for CodeIndexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_refresh: true,
            max_files: DEFAULT_MAX_FILES,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_source_bytes: DEFAULT_MAX_SOURCE_BYTES,
            max_chunks: DEFAULT_MAX_CHUNKS,
            chunk_lines: DEFAULT_CHUNK_LINES,
            overlap_lines: DEFAULT_OVERLAP_LINES,
            max_result_bytes: DEFAULT_MAX_RESULT_BYTES,
            embeddings: EmbeddingConfig::default(),
        }
    }
}

impl CodeIndexConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_range("code_index.max_files", self.max_files, 1, 100_000)?;
        validate_range(
            "code_index.max_file_bytes",
            self.max_file_bytes,
            4 * 1024,
            16 * 1024 * 1024,
        )?;
        validate_range(
            "code_index.max_source_bytes",
            self.max_source_bytes,
            self.max_file_bytes,
            1024 * 1024 * 1024,
        )?;
        validate_range("code_index.max_chunks", self.max_chunks, 1, 500_000)?;
        validate_range("code_index.chunk_lines", self.chunk_lines, 20, 500)?;
        if self.overlap_lines >= self.chunk_lines || self.overlap_lines > 100 {
            return Err(invalid_config(
                "code_index.overlap_lines",
                "must be at most 100 and smaller than chunk_lines",
            ));
        }
        validate_range(
            "code_index.max_result_bytes",
            self.max_result_bytes,
            4 * 1024,
            1024 * 1024,
        )?;
        self.embeddings.validate()?;
        Ok(())
    }
}

fn validate_range(
    field: &'static str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), ConfigError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid_config(
            field,
            format!("must be between {minimum} and {maximum}"),
        ));
    }
    Ok(())
}

fn invalid_config(field: &'static str, message: impl Into<String>) -> ConfigError {
    ConfigError::InvalidValue {
        field,
        message: message.into(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeIndexState {
    Disabled,
    Empty,
    Loading,
    Building,
    Ready,
    Cancelled,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CodeIndexSnapshot {
    pub runtime_available: bool,
    pub state: CodeIndexState,
    pub scanned_files: usize,
    pub total_files: usize,
    pub indexed_files: usize,
    pub chunk_count: usize,
    pub reused_files: usize,
    pub changed_files: usize,
    pub skipped_files: usize,
    pub source_bytes: usize,
    pub cache_bytes: u64,
    pub embeddings_enabled: bool,
    pub embedded_chunks: usize,
    pub vector_cache_bytes: u64,
    pub embedding_notice: UiNotice,
    pub generation: u64,
    pub last_built_at: Option<DateTime<Utc>>,
    pub notice: UiNotice,
}

impl CodeIndexSnapshot {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            runtime_available: enabled,
            state: if enabled {
                CodeIndexState::Empty
            } else {
                CodeIndexState::Disabled
            },
            scanned_files: 0,
            total_files: 0,
            indexed_files: 0,
            chunk_count: 0,
            reused_files: 0,
            changed_files: 0,
            skipped_files: 0,
            source_bytes: 0,
            cache_bytes: 0,
            embeddings_enabled: false,
            embedded_chunks: 0,
            vector_cache_bytes: 0,
            embedding_notice: UiNotice::EmbeddingDisabled,
            generation: 0,
            last_built_at: None,
            notice: if enabled {
                UiNotice::CodeIndexEmpty
            } else {
                UiNotice::CodeIndexDisabled
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CodeIndexHit {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub score: f64,
    pub symbols: Vec<String>,
    pub snippet: String,
}

#[derive(Debug, Error)]
pub enum CodeIndexError {
    #[error("repository index is disabled in trusted configuration")]
    Disabled,
    #[error("repository index is not ready: {0}")]
    NotReady(String),
    #[error("repository index refresh is already running")]
    AlreadyBuilding,
    #[error("repository index operation was cancelled")]
    Cancelled,
    #[error("repository index input is invalid: {0}")]
    InvalidInput(String),
    #[error("repository index cache at {path} is invalid: {message}")]
    InvalidCache { path: PathBuf, message: String },
    #[error("repository index I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("repository index JSON failed at {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("repository index worker failed: {0}")]
    Worker(String),
    #[error("repository embedding operation failed: {0}")]
    Embedding(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexChunk {
    start_line: usize,
    end_line: usize,
    text: String,
    symbols: Vec<String>,
    #[serde(skip, default)]
    terms: Vec<(String, u16)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct IndexedFile {
    path: String,
    size: u64,
    modified_nanos: Option<u128>,
    sha256: String,
    imports: Vec<String>,
    chunks: Vec<IndexChunk>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PersistedIndex {
    schema_version: u32,
    workspace_fingerprint: String,
    built_at: DateTime<Utc>,
    generation: u64,
    source_bytes: usize,
    files: Vec<IndexedFile>,
}

impl PersistedIndex {
    fn hydrate(&mut self) {
        for file in &mut self.files {
            for chunk in &mut file.chunks {
                chunk.terms = chunk_terms(&file.path, &file.imports, chunk);
            }
        }
    }

    fn chunk_count(&self) -> usize {
        self.files.iter().map(|file| file.chunks.len()).sum()
    }
}

#[derive(Debug)]
struct RuntimeIndex {
    persisted: PersistedIndex,
    inverted: HashMap<String, Vec<(usize, usize)>>,
}

impl RuntimeIndex {
    fn new(mut persisted: PersistedIndex) -> Self {
        persisted.hydrate();
        let mut inverted: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
        for (file_index, file) in persisted.files.iter().enumerate() {
            for (chunk_index, chunk) in file.chunks.iter().enumerate() {
                for (token, _) in &chunk.terms {
                    inverted
                        .entry(token.clone())
                        .or_default()
                        .push((file_index, chunk_index));
                }
            }
        }
        Self {
            persisted,
            inverted,
        }
    }

    fn search(&self, query: &str, path_filter: Option<&str>, top: usize) -> Vec<CodeIndexHit> {
        let query_tokens = tokenize(query);
        let normalized_filter = path_filter.map(normalize_filter);
        let mut candidates = BTreeSet::new();
        for token in &query_tokens {
            if let Some(locations) = self.inverted.get(token) {
                candidates.extend(locations.iter().copied());
            }
        }
        if candidates.is_empty() {
            for (file_index, file) in self.persisted.files.iter().enumerate() {
                if normalized_filter
                    .as_deref()
                    .is_some_and(|filter| !path_matches_filter(&file.path, filter))
                {
                    continue;
                }
                for chunk_index in 0..file.chunks.len() {
                    candidates.insert((file_index, chunk_index));
                }
            }
        }

        let total_chunks = self.persisted.chunk_count().max(1) as f64;
        let lowered_query = query.to_lowercase();
        let mut hits = candidates
            .into_iter()
            .filter_map(|(file_index, chunk_index)| {
                let file = self.persisted.files.get(file_index)?;
                if normalized_filter
                    .as_deref()
                    .is_some_and(|filter| !path_matches_filter(&file.path, filter))
                {
                    return None;
                }
                let chunk = file.chunks.get(chunk_index)?;
                let lowered_text = chunk.text.to_lowercase();
                let mut score = if lowered_text.contains(&lowered_query) {
                    14.0
                } else {
                    0.0
                };
                for token in &query_tokens {
                    let frequency = chunk
                        .terms
                        .binary_search_by(|(candidate, _)| candidate.cmp(token))
                        .ok()
                        .and_then(|index| chunk.terms.get(index))
                        .map_or(0, |(_, count)| usize::from(*count));
                    if frequency == 0 {
                        continue;
                    }
                    let document_frequency = self
                        .inverted
                        .get(token)
                        .map_or(1.0, |locations| locations.len().max(1) as f64);
                    let idf = ((total_chunks + 1.0) / (document_frequency + 1.0)).ln() + 1.0;
                    score += (1.0 + (frequency as f64).ln()) * idf;
                    if file.path.to_lowercase().contains(token) {
                        score += 2.5;
                    }
                    if chunk
                        .symbols
                        .iter()
                        .any(|symbol| symbol.to_lowercase().contains(token))
                    {
                        score += 5.0;
                    }
                    if file
                        .imports
                        .iter()
                        .any(|import| import.to_lowercase().contains(token))
                    {
                        score += 1.5;
                    }
                }
                (score > 0.0).then(|| CodeIndexHit {
                    path: file.path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    symbols: chunk.symbols.clone(),
                    snippet: sanitize_text(&chunk.text, MAX_SNIPPET_BYTES),
                })
            })
            .collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.start_line.cmp(&right.start_line))
        });
        hits.truncate(top);
        hits
    }

    fn hybrid_search(
        &self,
        query: &str,
        query_vector: &[f32],
        vectors: &VectorIndex,
        path_filter: Option<&str>,
        top: usize,
        vector_weight: f32,
    ) -> Vec<CodeIndexHit> {
        let lexical = self.search(query, path_filter, self.persisted.chunk_count());
        let lexical_max = lexical.first().map_or(1.0, |hit| hit.score.max(1.0));
        let lexical_scores = lexical
            .into_iter()
            .map(|hit| {
                (
                    (hit.path, hit.start_line, hit.end_line),
                    hit.score / lexical_max,
                )
            })
            .collect::<HashMap<_, _>>();
        let normalized_filter = path_filter.map(normalize_filter);
        let vector_weight = f64::from(vector_weight);
        let lexical_weight = 1.0 - vector_weight;
        let mut hits = Vec::new();
        for file in &self.persisted.files {
            if normalized_filter
                .as_deref()
                .is_some_and(|filter| !path_matches_filter(&file.path, filter))
            {
                continue;
            }
            for chunk in &file.chunks {
                let key = vector_key(&file.path, chunk.start_line, chunk.end_line, &chunk.text);
                let lexical_score = lexical_scores
                    .get(&(file.path.clone(), chunk.start_line, chunk.end_line))
                    .copied()
                    .unwrap_or_default();
                let vector_score = vectors
                    .score(query_vector, &key)
                    .map(|score| (score + 1.0) / 2.0);
                if lexical_score == 0.0 && vector_score.is_none() {
                    continue;
                }
                let score = lexical_weight * lexical_score
                    + vector_weight * vector_score.unwrap_or_default();
                hits.push(CodeIndexHit {
                    path: file.path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    score,
                    symbols: chunk.symbols.clone(),
                    snippet: sanitize_text(&chunk.text, MAX_SNIPPET_BYTES),
                });
            }
        }
        hits.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.start_line.cmp(&right.start_line))
        });
        hits.truncate(top);
        hits
    }
}

#[derive(Debug)]
struct SharedProgress {
    snapshot: Mutex<CodeIndexSnapshot>,
}

impl SharedProgress {
    fn new(enabled: bool) -> Self {
        Self {
            snapshot: Mutex::new(CodeIndexSnapshot::new(enabled)),
        }
    }

    fn snapshot(&self) -> CodeIndexSnapshot {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_else(|_| {
                let mut snapshot = CodeIndexSnapshot::new(true);
                snapshot.state = CodeIndexState::Error;
                snapshot.notice = UiNotice::DependencyFailure;
                snapshot
            })
    }

    fn update(&self, update: impl FnOnce(&mut CodeIndexSnapshot)) {
        let Ok(mut snapshot) = self.snapshot.lock() else {
            tracing::error!("code index status lock was poisoned");
            return;
        };
        update(&mut snapshot);
    }
}

struct BuildTask {
    cancel: CancellationToken,
    handle: JoinHandle<Result<BuildOutput, CodeIndexError>>,
    force: bool,
}

struct EmbeddingBuildTask {
    cancel: CancellationToken,
    handle: JoinHandle<Result<EmbeddingBuildOutput, CodeIndexError>>,
}

struct BuildOutput {
    runtime: RuntimeIndex,
    cache_bytes: u64,
    reused_files: usize,
    changed_files: usize,
    skipped_files: usize,
}

struct BuildBaseline<'a> {
    previous: Option<&'a RuntimeIndex>,
    privacy: Option<&'a PrivacyShield>,
}

pub struct CodeIndexManager {
    config: CodeIndexConfig,
    root: PathBuf,
    storage_root: PathBuf,
    privacy: Option<PrivacyShield>,
    privacy_sha256: String,
    workspace_fingerprint: String,
    cache_path: PathBuf,
    vector_cache_path: PathBuf,
    runtime: Arc<RwLock<Option<Arc<RuntimeIndex>>>>,
    vectors: Arc<RwLock<Option<Arc<VectorIndex>>>>,
    progress: Arc<SharedProgress>,
    build: Option<BuildTask>,
    embedding_build: Option<EmbeddingBuildTask>,
}

impl CodeIndexManager {
    pub fn new(
        config: CodeIndexConfig,
        workspace_root: &Path,
        storage_root: &Path,
    ) -> Result<Self, ConfigError> {
        let privacy = PrivacyShield::load_project_only(workspace_root).ok();
        Self::new_inner(config, workspace_root, storage_root, privacy)
    }

    pub(crate) fn new_with_privacy(
        config: CodeIndexConfig,
        workspace_root: &Path,
        storage_root: &Path,
        privacy: PrivacyShield,
    ) -> Result<Self, ConfigError> {
        Self::new_inner(config, workspace_root, storage_root, Some(privacy))
    }

    fn new_inner(
        config: CodeIndexConfig,
        workspace_root: &Path,
        storage_root: &Path,
        privacy: Option<PrivacyShield>,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let root = dunce::canonicalize(workspace_root).map_err(|source| ConfigError::PathIo {
            field: "agent.workspace_root",
            path: workspace_root.to_path_buf(),
            source,
        })?;
        let privacy_sha256 = privacy
            .as_ref()
            .and_then(|shield| shield.policy_sha256().ok())
            .unwrap_or_else(|| "privacy-unavailable".to_owned());
        let workspace_fingerprint = workspace_fingerprint(&root, &privacy_sha256, &config);
        let cache_path = storage_root
            .join("code-indexes")
            .join(&workspace_fingerprint)
            .join("index.json");
        let vector_cache_path = storage_root
            .join("code-indexes")
            .join(&workspace_fingerprint)
            .join(format!("vectors-{}.json", config.embeddings.fingerprint()));
        let progress = Arc::new(SharedProgress::new(config.enabled));
        progress.update(|snapshot| {
            snapshot.embeddings_enabled = config.embeddings.enabled;
            snapshot.embedding_notice = if config.embeddings.enabled {
                UiNotice::EmbeddingConfigured
            } else {
                UiNotice::EmbeddingDisabled
            };
        });
        Ok(Self {
            progress,
            config,
            root,
            storage_root: storage_root.to_path_buf(),
            privacy,
            privacy_sha256,
            workspace_fingerprint,
            cache_path,
            vector_cache_path,
            runtime: Arc::new(RwLock::new(None)),
            vectors: Arc::new(RwLock::new(None)),
            build: None,
            embedding_build: None,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> CodeIndexSnapshot {
        self.progress.snapshot()
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    #[must_use]
    pub fn function_definitions(&self) -> Vec<FunctionToolDefinition> {
        if !self.config.enabled {
            return Vec::new();
        }
        code_index_function_definitions()
    }

    pub async fn start(&mut self) {
        if !self.config.enabled {
            return;
        }
        self.progress.update(|snapshot| {
            snapshot.state = CodeIndexState::Loading;
            snapshot.notice = UiNotice::CodeIndexLoading;
        });
        match load_cache(self.cache_path.clone(), self.workspace_fingerprint.clone()).await {
            Ok(Some((runtime, cache_bytes))) => self.install_runtime(runtime, cache_bytes, 0, 0, 0),
            Ok(None) => self.progress.update(|snapshot| {
                snapshot.state = CodeIndexState::Empty;
                snapshot.notice = UiNotice::CodeIndexEmpty;
            }),
            Err(error) => {
                tracing::warn!(%error, "repository index cache was ignored");
                self.progress.update(|snapshot| {
                    snapshot.state = CodeIndexState::Error;
                    snapshot.notice = UiNotice::external(sanitize_text(&error.to_string(), 2_048));
                });
            }
        }
        if self.config.embeddings.enabled {
            self.load_existing_vectors().await;
        }
        if self.config.auto_refresh
            && let Err(error) = self.start_refresh(false)
        {
            tracing::warn!(%error, "automatic repository index refresh did not start");
        }
    }

    pub fn start_refresh(&mut self, force: bool) -> Result<(), CodeIndexError> {
        if !self.config.enabled {
            return Err(CodeIndexError::Disabled);
        }
        if self.build.is_some() || self.embedding_build.is_some() {
            return Err(CodeIndexError::AlreadyBuilding);
        }
        let previous = if force {
            None
        } else {
            self.runtime.read().ok().and_then(|runtime| runtime.clone())
        };
        let config = self.config.clone();
        let root = self.root.clone();
        let fingerprint = self.workspace_fingerprint.clone();
        let cache_path = self.cache_path.clone();
        let progress = Arc::clone(&self.progress);
        let privacy = self.privacy.clone();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        progress.update(|snapshot| {
            snapshot.state = CodeIndexState::Building;
            snapshot.scanned_files = 0;
            snapshot.total_files = 0;
            snapshot.reused_files = 0;
            snapshot.changed_files = 0;
            snapshot.skipped_files = 0;
            snapshot.notice = if force {
                UiNotice::CodeIndexRebuilding
            } else {
                UiNotice::CodeIndexRefreshing
            };
        });
        let handle = tokio::task::spawn_blocking(move || {
            build_index(
                &config,
                &root,
                &fingerprint,
                &cache_path,
                BuildBaseline {
                    previous: previous.as_deref(),
                    privacy: privacy.as_ref(),
                },
                &progress,
                &worker_cancel,
            )
        });
        self.build = Some(BuildTask {
            cancel,
            handle,
            force,
        });
        Ok(())
    }

    pub fn cancel_refresh(&self) -> Result<(), CodeIndexError> {
        let lexical = self.build.as_ref();
        let vectors = self.embedding_build.as_ref();
        if lexical.is_none() && vectors.is_none() {
            return Err(CodeIndexError::NotReady(
                "no index refresh is running".to_owned(),
            ));
        }
        if let Some(build) = lexical {
            build.cancel.cancel();
        }
        if let Some(build) = vectors {
            build.cancel.cancel();
        }
        self.progress.update(|snapshot| {
            snapshot.notice = UiNotice::CodeIndexCancelling;
            if snapshot.embeddings_enabled {
                snapshot.embedding_notice = UiNotice::EmbeddingCancelling;
            }
        });
        Ok(())
    }

    pub async fn poll(&mut self) {
        if self
            .build
            .as_ref()
            .is_some_and(|build| build.handle.is_finished())
            && let Some(build) = self.build.take()
        {
            match build.handle.await {
                Ok(Ok(output)) => {
                    self.install_runtime(
                        output.runtime,
                        output.cache_bytes,
                        output.reused_files,
                        output.changed_files,
                        output.skipped_files,
                    );
                    if self.config.embeddings.enabled
                        && let Err(error) = self.start_embedding_refresh(build.force)
                    {
                        self.embedding_failed(error);
                    }
                }
                Ok(Err(CodeIndexError::Cancelled)) => self.progress.update(|snapshot| {
                    snapshot.state = CodeIndexState::Cancelled;
                    snapshot.notice = UiNotice::CodeIndexCancelled;
                }),
                Ok(Err(error)) => {
                    tracing::warn!(%error, "repository index refresh failed");
                    self.progress.update(|snapshot| {
                        snapshot.state = CodeIndexState::Error;
                        snapshot.notice =
                            UiNotice::external(sanitize_text(&error.to_string(), 2_048));
                    });
                }
                Err(error) => self.progress.update(|snapshot| {
                    snapshot.state = CodeIndexState::Error;
                    tracing::error!(%error, "repository index worker failed");
                    snapshot.notice = UiNotice::DependencyFailure;
                }),
            }
        }
        if self
            .embedding_build
            .as_ref()
            .is_some_and(|build| build.handle.is_finished())
            && let Some(build) = self.embedding_build.take()
        {
            match build.handle.await {
                Ok(Ok(output)) => self.install_vectors(output),
                Ok(Err(CodeIndexError::Cancelled)) => self.progress.update(|snapshot| {
                    snapshot.embedding_notice = UiNotice::EmbeddingCancelling;
                }),
                Ok(Err(error)) => self.embedding_failed(error),
                Err(error) => self.embedding_failed(CodeIndexError::Worker(error.to_string())),
            }
        }
    }

    pub async fn shutdown(&mut self) {
        if let Some(build) = self.build.take() {
            stop_task(build.cancel, build.handle).await;
        }
        if let Some(build) = self.embedding_build.take() {
            stop_task(build.cancel, build.handle).await;
        }
    }

    pub fn privacy_reloaded(&mut self) -> Result<bool, CodeIndexError> {
        let next_sha256 = self
            .privacy
            .as_ref()
            .ok_or_else(|| CodeIndexError::NotReady("privacy policy is unavailable".to_owned()))?
            .policy_sha256()
            .map_err(|error| CodeIndexError::NotReady(error.to_string()))?;
        if next_sha256 == self.privacy_sha256 {
            return Ok(false);
        }
        if let Some(build) = self.build.take() {
            build.cancel.cancel();
            build.handle.abort();
        }
        if let Some(build) = self.embedding_build.take() {
            build.cancel.cancel();
            build.handle.abort();
        }
        self.privacy_sha256 = next_sha256;
        self.workspace_fingerprint =
            workspace_fingerprint(&self.root, &self.privacy_sha256, &self.config);
        self.cache_path = self
            .storage_root
            .join("code-indexes")
            .join(&self.workspace_fingerprint)
            .join("index.json");
        self.vector_cache_path = self
            .storage_root
            .join("code-indexes")
            .join(&self.workspace_fingerprint)
            .join(format!(
                "vectors-{}.json",
                self.config.embeddings.fingerprint()
            ));
        self.runtime
            .write()
            .map_err(|_| CodeIndexError::NotReady("index lock was poisoned".to_owned()))?
            .take();
        self.vectors
            .write()
            .map_err(|_| CodeIndexError::NotReady("vector index lock was poisoned".to_owned()))?
            .take();
        self.progress.update(|snapshot| {
            snapshot.state = CodeIndexState::Empty;
            snapshot.indexed_files = 0;
            snapshot.chunk_count = 0;
            snapshot.source_bytes = 0;
            snapshot.cache_bytes = 0;
            snapshot.embedded_chunks = 0;
            snapshot.vector_cache_bytes = 0;
            if snapshot.embeddings_enabled {
                snapshot.embedding_notice = UiNotice::EmbeddingPrivacyRefresh;
            }
            snapshot.notice = if self.build.is_some() {
                UiNotice::CodeIndexCancelling
            } else {
                UiNotice::CodeIndexEmpty
            };
        });
        Ok(true)
    }

    pub async fn call(
        &mut self,
        function: &str,
        arguments: &str,
        cancel: &CancellationToken,
    ) -> Result<String, CodeIndexError> {
        if !self.config.enabled {
            return Err(CodeIndexError::Disabled);
        }
        self.poll().await;
        if cancel.is_cancelled() {
            return Err(CodeIndexError::Cancelled);
        }
        if function == INDEX_STATUS_TOOL {
            let _: EmptyArguments = parse_arguments(arguments)?;
            return bounded_json(
                &json!({ "index": self.snapshot() }),
                self.config.max_result_bytes,
            );
        }
        let runtime = self
            .runtime
            .read()
            .map_err(|_| CodeIndexError::NotReady("index lock was poisoned".to_owned()))?
            .clone()
            .ok_or_else(|| CodeIndexError::NotReady("repository index is not ready".to_owned()))?;
        let value = match function {
            INDEX_SEARCH_TOOL => {
                let arguments: SearchArguments = parse_arguments(arguments)?;
                validate_query(&arguments.query)?;
                let top = arguments.top.clamp(1, MAX_SEARCH_RESULTS);
                let path = arguments
                    .path
                    .as_deref()
                    .map(validate_relative_filter)
                    .transpose()?;
                let hits = self
                    .ranked_search(&runtime, &arguments.query, path.as_deref(), top, cancel)
                    .await?;
                json!({ "query": arguments.query, "hits": hits })
            }
            INDEX_OVERVIEW_TOOL => {
                let arguments: OverviewArguments = parse_arguments(arguments)?;
                let path = arguments
                    .path
                    .as_deref()
                    .map(validate_relative_filter)
                    .transpose()?;
                repository_overview(&runtime.persisted, path.as_deref())
            }
            INDEX_DEPENDENCIES_TOOL => {
                let arguments: DependenciesArguments = parse_arguments(arguments)?;
                let path = validate_relative_filter(&arguments.path)?;
                repository_dependencies(&runtime.persisted, &path)?
            }
            _ => {
                return Err(CodeIndexError::InvalidInput(format!(
                    "unknown native index function {function:?}"
                )));
            }
        };
        if cancel.is_cancelled() {
            return Err(CodeIndexError::Cancelled);
        }
        bounded_json(&value, self.config.max_result_bytes)
    }

    pub async fn search(
        &mut self,
        query: &str,
        path: Option<&str>,
        top: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<CodeIndexHit>, CodeIndexError> {
        if !self.config.enabled {
            return Err(CodeIndexError::Disabled);
        }
        self.poll().await;
        validate_query(query)?;
        let path = path.map(validate_relative_filter).transpose()?;
        if cancel.is_cancelled() {
            return Err(CodeIndexError::Cancelled);
        }
        let runtime = self
            .runtime
            .read()
            .map_err(|_| CodeIndexError::NotReady("index lock was poisoned".to_owned()))?
            .clone()
            .ok_or_else(|| CodeIndexError::NotReady("repository index is not ready".to_owned()))?;
        self.ranked_search(
            &runtime,
            query,
            path.as_deref(),
            top.clamp(1, MAX_SEARCH_RESULTS),
            cancel,
        )
        .await
    }

    async fn ranked_search(
        &self,
        runtime: &RuntimeIndex,
        query: &str,
        path: Option<&str>,
        top: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<CodeIndexHit>, CodeIndexError> {
        if !self.config.embeddings.enabled {
            return Ok(runtime.search(query, path, top));
        }
        let vectors = self
            .vectors
            .read()
            .map_err(|_| CodeIndexError::NotReady("vector index lock was poisoned".to_owned()))?
            .clone();
        let Some(vectors) = vectors else {
            return Ok(runtime.search(query, path, top));
        };
        let client = EmbeddingClient::new(self.config.embeddings.clone())?;
        let inputs = [query.to_owned()];
        let query_vector = match client.embed(&inputs, cancel).await {
            Ok(mut values) => values.pop().ok_or_else(|| {
                CodeIndexError::Embedding("query embedding response was empty".to_owned())
            })?,
            Err(CodeIndexError::Cancelled) => return Err(CodeIndexError::Cancelled),
            Err(error) => {
                tracing::warn!(%error, "vector query failed; using lexical fallback");
                self.progress.update(|snapshot| {
                    snapshot.embedding_notice = UiNotice::LexicalFallback {
                        detail: sanitize_text(&error.to_string(), 512),
                    };
                });
                return Ok(runtime.search(query, path, top));
            }
        };
        Ok(runtime.hybrid_search(
            query,
            &query_vector,
            &vectors,
            path,
            top,
            self.config.embeddings.hybrid_weight,
        ))
    }

    async fn load_existing_vectors(&mut self) {
        let runtime = self.runtime.read().ok().and_then(|runtime| runtime.clone());
        let Some(runtime) = runtime else {
            return;
        };
        let chunks = embedding_chunks(&runtime, &self.config.embeddings);
        let keys = chunks
            .into_iter()
            .map(|chunk| chunk.key)
            .collect::<BTreeSet<_>>();
        match load_vector_cache(
            self.vector_cache_path.clone(),
            self.workspace_fingerprint.clone(),
            self.config.embeddings.fingerprint(),
            keys,
        )
        .await
        {
            Ok(Some((index, cache_bytes))) => {
                let count = index.len();
                if let Ok(mut vectors) = self.vectors.write() {
                    *vectors = Some(Arc::new(index));
                }
                self.progress.update(|snapshot| {
                    snapshot.embedded_chunks = count;
                    snapshot.vector_cache_bytes = cache_bytes;
                    snapshot.embedding_notice = UiNotice::EmbeddingReady {
                        count,
                        reused: count,
                        embedded: 0,
                    };
                });
            }
            Ok(None) => self.progress.update(|snapshot| {
                snapshot.embedding_notice = UiNotice::EmbeddingCacheMissing;
            }),
            Err(error) => self.embedding_failed(error),
        }
    }

    fn start_embedding_refresh(&mut self, force: bool) -> Result<(), CodeIndexError> {
        if !self.config.embeddings.enabled {
            return Ok(());
        }
        if self.embedding_build.is_some() {
            return Err(CodeIndexError::AlreadyBuilding);
        }
        let runtime = self
            .runtime
            .read()
            .map_err(|_| CodeIndexError::NotReady("index lock was poisoned".to_owned()))?
            .clone()
            .ok_or_else(|| CodeIndexError::NotReady("lexical index is unavailable".to_owned()))?;
        let chunks = embedding_chunks(&runtime, &self.config.embeddings);
        if chunks.is_empty() {
            self.progress.update(|snapshot| {
                snapshot.embedding_notice = UiNotice::EmbeddingNoChunks;
            });
            return Ok(());
        }
        let client = EmbeddingClient::new(self.config.embeddings.clone())?;
        let previous = if force {
            None
        } else {
            self.vectors.read().ok().and_then(|vectors| vectors.clone())
        };
        let cache_path = self.vector_cache_path.clone();
        let workspace_fingerprint = self.workspace_fingerprint.clone();
        let embedding_fingerprint = self.config.embeddings.fingerprint();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        self.progress.update(|snapshot| {
            snapshot.embedding_notice = UiNotice::EmbeddingConfigured;
        });
        let handle = tokio::spawn(async move {
            build_vector_index(
                client,
                chunks,
                previous,
                cache_path,
                workspace_fingerprint,
                embedding_fingerprint,
                worker_cancel,
            )
            .await
        });
        self.embedding_build = Some(EmbeddingBuildTask { cancel, handle });
        Ok(())
    }

    fn install_vectors(&mut self, output: EmbeddingBuildOutput) {
        let count = output.index.len();
        if let Ok(mut vectors) = self.vectors.write() {
            *vectors = Some(Arc::new(output.index));
        } else {
            self.embedding_failed(CodeIndexError::NotReady(
                "vector index lock was poisoned".to_owned(),
            ));
            return;
        }
        self.progress.update(|snapshot| {
            snapshot.embedded_chunks = count;
            snapshot.vector_cache_bytes = output.cache_bytes;
            snapshot.embedding_notice = UiNotice::EmbeddingReady {
                count,
                reused: output.reused,
                embedded: output.embedded,
            };
        });
    }

    fn embedding_failed(&self, error: CodeIndexError) {
        tracing::warn!(%error, "repository embedding operation failed; lexical index retained");
        self.progress.update(|snapshot| {
            snapshot.embedding_notice = UiNotice::LexicalFallback {
                detail: sanitize_text(&error.to_string(), 1_024),
            };
        });
    }

    fn install_runtime(
        &mut self,
        runtime: RuntimeIndex,
        cache_bytes: u64,
        reused_files: usize,
        changed_files: usize,
        skipped_files: usize,
    ) {
        let indexed_files = runtime.persisted.files.len();
        let chunk_count = runtime.persisted.chunk_count();
        let source_bytes = runtime.persisted.source_bytes;
        let generation = runtime.persisted.generation;
        let last_built_at = runtime.persisted.built_at;
        if let Ok(mut current) = self.runtime.write() {
            *current = Some(Arc::new(runtime));
        } else {
            tracing::error!("repository index runtime lock was poisoned");
        }
        self.progress.update(|snapshot| {
            snapshot.state = CodeIndexState::Ready;
            snapshot.scanned_files = indexed_files.saturating_add(skipped_files);
            snapshot.total_files = snapshot.scanned_files;
            snapshot.indexed_files = indexed_files;
            snapshot.chunk_count = chunk_count;
            snapshot.reused_files = reused_files;
            snapshot.changed_files = changed_files;
            snapshot.skipped_files = skipped_files;
            snapshot.source_bytes = source_bytes;
            snapshot.cache_bytes = cache_bytes;
            snapshot.generation = generation;
            snapshot.last_built_at = Some(last_built_at);
            snapshot.notice = UiNotice::CodeIndexReady;
        });
    }
}

async fn stop_task<T>(cancel: CancellationToken, mut handle: JoinHandle<Result<T, CodeIndexError>>)
where
    T: Send + 'static,
{
    cancel.cancel();
    if tokio::time::timeout(SHUTDOWN_GRACE, &mut handle)
        .await
        .is_err()
    {
        handle.abort();
        let _ = handle.await;
    }
}

fn embedding_chunks(runtime: &RuntimeIndex, config: &EmbeddingConfig) -> Vec<EmbeddingChunk> {
    runtime
        .persisted
        .files
        .iter()
        .flat_map(|file| {
            file.chunks.iter().map(|chunk| {
                let key = vector_key(&file.path, chunk.start_line, chunk.end_line, &chunk.text);
                let input = format!(
                    "path: {}\nlines: {}-{}\nsymbols: {}\n{}",
                    file.path,
                    chunk.start_line,
                    chunk.end_line,
                    chunk.symbols.join(", "),
                    chunk.text
                );
                EmbeddingChunk {
                    key,
                    input: bounded_embedding_input(&input, config.max_input_bytes),
                }
            })
        })
        .take(config.max_chunks)
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArguments {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArguments {
    query: String,
    path: Option<String>,
    top: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewArguments {
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DependenciesArguments {
    path: String,
}

fn parse_arguments<T: DeserializeOwned>(arguments: &str) -> Result<T, CodeIndexError> {
    serde_json::from_str(arguments).map_err(|error| {
        CodeIndexError::InvalidInput(format!("arguments do not match the strict schema: {error}"))
    })
}

fn validate_query(query: &str) -> Result<(), CodeIndexError> {
    if query.trim().is_empty() || query.len() > MAX_QUERY_BYTES || query.contains('\0') {
        return Err(CodeIndexError::InvalidInput(format!(
            "query must be non-empty, NUL-free, and at most {MAX_QUERY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_relative_filter(path: &str) -> Result<String, CodeIndexError> {
    if path.is_empty() || path.len() > MAX_PATH_FILTER_BYTES || path.contains('\0') {
        return Err(CodeIndexError::InvalidInput(format!(
            "path filter must be non-empty, NUL-free, and at most {MAX_PATH_FILTER_BYTES} bytes"
        )));
    }
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(CodeIndexError::InvalidInput(
            "path filter must stay within the workspace".to_owned(),
        ));
    }
    Ok(normalize_filter(&path.to_string_lossy()))
}

fn normalize_filter(path: &str) -> String {
    path.replace('\\', "/")
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect::<Vec<_>>()
        .join("/")
}

fn path_matches_filter(path: &str, filter: &str) -> bool {
    filter.is_empty()
        || path == filter
        || path
            .strip_prefix(filter)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

async fn load_cache(
    path: PathBuf,
    workspace_fingerprint: String,
) -> Result<Option<(RuntimeIndex, u64)>, CodeIndexError> {
    tokio::task::spawn_blocking(move || {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(CodeIndexError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        if metadata.len() > MAX_CACHE_BYTES {
            return Err(CodeIndexError::InvalidCache {
                path: path.clone(),
                message: format!("cache exceeds the {MAX_CACHE_BYTES} byte safety limit"),
            });
        }
        let bytes = fs::read(&path).map_err(|source| CodeIndexError::Io {
            path: path.clone(),
            source,
        })?;
        let persisted: PersistedIndex =
            serde_json::from_slice(&bytes).map_err(|source| CodeIndexError::Json {
                path: path.clone(),
                source,
            })?;
        if persisted.schema_version != SCHEMA_VERSION
            || persisted.workspace_fingerprint != workspace_fingerprint
        {
            return Err(CodeIndexError::InvalidCache {
                path,
                message: "schema or workspace fingerprint does not match".to_owned(),
            });
        }
        Ok(Some((RuntimeIndex::new(persisted), metadata.len())))
    })
    .await
    .map_err(|error| CodeIndexError::Worker(error.to_string()))?
}

fn build_index(
    config: &CodeIndexConfig,
    root: &Path,
    workspace_fingerprint: &str,
    cache_path: &Path,
    baseline: BuildBaseline<'_>,
    progress: &SharedProgress,
    cancel: &CancellationToken,
) -> Result<BuildOutput, CodeIndexError> {
    check_cancel(cancel)?;
    let mut walker = WalkBuilder::new(root);
    walker
        .follow_links(false)
        .standard_filters(true)
        .require_git(false);
    let mut candidates = walker
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter(|entry| {
            !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with('.'))
        })
        .filter_map(|entry| {
            entry.path().strip_prefix(root).ok().map(|relative| {
                (
                    entry.path().to_path_buf(),
                    relative.to_string_lossy().replace('\\', "/"),
                )
            })
        })
        .filter(|(_, relative)| {
            baseline
                .privacy
                .is_none_or(|shield| shield.allows_relative(Path::new(relative), false))
        })
        .take(config.max_files.saturating_add(1))
        .collect::<Vec<_>>();
    let hit_file_limit = candidates.len() > config.max_files;
    candidates.truncate(config.max_files);
    candidates.sort_by(|left, right| left.1.cmp(&right.1));
    let candidate_count = candidates.len();
    progress.update(|snapshot| {
        snapshot.total_files = candidates.len();
        snapshot.notice = UiNotice::CodeIndexScanning {
            count: candidates.len(),
        };
    });

    let previous_files = baseline
        .previous
        .map(|runtime| {
            runtime
                .persisted
                .files
                .iter()
                .map(|file| (file.path.as_str(), file))
                .collect::<HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut files = Vec::new();
    let mut source_bytes = 0_usize;
    let mut chunk_count = 0_usize;
    let mut reused_files = 0_usize;
    let mut changed_files = 0_usize;
    let mut skipped_files = usize::from(hit_file_limit);

    for (index, (absolute, relative)) in candidates.into_iter().enumerate() {
        check_cancel(cancel)?;
        let metadata = match fs::metadata(&absolute) {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped_files = skipped_files.saturating_add(1);
                continue;
            }
        };
        let size = metadata.len();
        if size == 0 || size > config.max_file_bytes as u64 {
            skipped_files = skipped_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        }
        let size_usize = usize::try_from(size).unwrap_or(usize::MAX);
        if source_bytes.saturating_add(size_usize) > config.max_source_bytes {
            skipped_files = skipped_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        }
        let modified_nanos = metadata.modified().ok().and_then(system_time_nanos);
        let bytes = match fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(_) => {
                skipped_files = skipped_files.saturating_add(1);
                update_build_progress(
                    progress,
                    cancel,
                    index + 1,
                    reused_files,
                    changed_files,
                    skipped_files,
                );
                continue;
            }
        };
        if is_binary(&bytes) {
            skipped_files = skipped_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        }
        let sha256 = format!("{:x}", Sha256::digest(&bytes));
        if let Some(previous_file) = previous_files.get(relative.as_str())
            && previous_file.sha256 == sha256
            && chunk_count.saturating_add(previous_file.chunks.len()) <= config.max_chunks
        {
            let mut reused = (*previous_file).clone();
            reused.size = size;
            reused.modified_nanos = modified_nanos;
            source_bytes = source_bytes.saturating_add(size_usize);
            chunk_count = chunk_count.saturating_add(reused.chunks.len());
            files.push(reused);
            reused_files = reused_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            skipped_files = skipped_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        };
        let imports = extract_imports(&text);
        let mut chunks = make_chunks(&relative, &text, &imports, config);
        if chunk_count.saturating_add(chunks.len()) > config.max_chunks {
            let remaining = config.max_chunks.saturating_sub(chunk_count);
            chunks.truncate(remaining);
        }
        if chunks.is_empty() {
            skipped_files = skipped_files.saturating_add(1);
            update_build_progress(
                progress,
                cancel,
                index + 1,
                reused_files,
                changed_files,
                skipped_files,
            );
            continue;
        }
        source_bytes = source_bytes.saturating_add(size_usize);
        chunk_count = chunk_count.saturating_add(chunks.len());
        files.push(IndexedFile {
            path: relative,
            size,
            modified_nanos,
            sha256,
            imports,
            chunks,
        });
        changed_files = changed_files.saturating_add(1);
        update_build_progress(
            progress,
            cancel,
            index + 1,
            reused_files,
            changed_files,
            skipped_files,
        );
        if chunk_count >= config.max_chunks {
            skipped_files = skipped_files
                .saturating_add(candidate_count.saturating_sub(index.saturating_add(1)));
            break;
        }
    }
    check_cancel(cancel)?;
    let generation = baseline
        .previous
        .map_or(1, |runtime| runtime.persisted.generation.saturating_add(1));
    let persisted = PersistedIndex {
        schema_version: SCHEMA_VERSION,
        workspace_fingerprint: workspace_fingerprint.to_owned(),
        built_at: Utc::now(),
        generation,
        source_bytes,
        files,
    };
    let cache_bytes = persist_index(cache_path, &persisted, cancel)?;
    Ok(BuildOutput {
        runtime: RuntimeIndex::new(persisted),
        cache_bytes,
        reused_files,
        changed_files,
        skipped_files,
    })
}

fn update_build_progress(
    progress: &SharedProgress,
    cancel: &CancellationToken,
    scanned: usize,
    reused: usize,
    changed: usize,
    skipped: usize,
) {
    if cancel.is_cancelled() {
        return;
    }
    progress.update(|snapshot| {
        snapshot.scanned_files = scanned;
        snapshot.reused_files = reused;
        snapshot.changed_files = changed;
        snapshot.skipped_files = skipped;
        snapshot.notice = UiNotice::CodeIndexProgress {
            scanned,
            reused,
            changed,
            skipped,
        };
    });
}

fn persist_index(
    path: &Path,
    persisted: &PersistedIndex,
    cancel: &CancellationToken,
) -> Result<u64, CodeIndexError> {
    check_cancel(cancel)?;
    let parent = path.parent().ok_or_else(|| CodeIndexError::InvalidCache {
        path: path.to_path_buf(),
        message: "cache path has no parent directory".to_owned(),
    })?;
    fs::create_dir_all(parent).map_err(|source| CodeIndexError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| CodeIndexError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    serde_json::to_writer(&mut temporary, persisted).map_err(|source| CodeIndexError::Json {
        path: temporary.path().to_path_buf(),
        source,
    })?;
    temporary.flush().map_err(|source| CodeIndexError::Io {
        path: temporary.path().to_path_buf(),
        source,
    })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| CodeIndexError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    check_cancel(cancel)?;
    let length = temporary
        .as_file()
        .metadata()
        .map_err(|source| CodeIndexError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?
        .len();
    if length > MAX_CACHE_BYTES {
        return Err(CodeIndexError::InvalidCache {
            path: path.to_path_buf(),
            message: format!(
                "generated cache is {length} bytes and exceeds the {MAX_CACHE_BYTES} byte safety limit; lower code_index limits"
            ),
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

fn check_cancel(cancel: &CancellationToken) -> Result<(), CodeIndexError> {
    if cancel.is_cancelled() {
        Err(CodeIndexError::Cancelled)
    } else {
        Ok(())
    }
}

fn system_time_nanos(value: SystemTime) -> Option<u128> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8 * 1024).any(|byte| *byte == 0)
}

fn make_chunks(
    path: &str,
    text: &str,
    imports: &[String],
    config: &CodeIndexConfig,
) -> Vec<IndexChunk> {
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return Vec::new();
    }
    let step = config
        .chunk_lines
        .saturating_sub(config.overlap_lines)
        .max(1);
    let mut chunks = Vec::new();
    let mut start = 0_usize;
    while start < lines.len() {
        let end = start.saturating_add(config.chunk_lines).min(lines.len());
        let chunk_text = lines[start..end].join("\n");
        let symbols = extract_symbols(&chunk_text);
        let mut chunk = IndexChunk {
            start_line: start.saturating_add(1),
            end_line: end,
            text: chunk_text,
            symbols,
            terms: Vec::new(),
        };
        chunk.terms = chunk_terms(path, imports, &chunk);
        chunks.push(chunk);
        if end == lines.len() {
            break;
        }
        start = start.saturating_add(step);
    }
    chunks
}

fn extract_symbols(text: &str) -> Vec<String> {
    let prefixes = [
        "fn ",
        "async fn ",
        "pub fn ",
        "pub async fn ",
        "pub(crate) fn ",
        "pub(crate) async fn ",
        "pub(super) fn ",
        "pub(super) async fn ",
        "struct ",
        "enum ",
        "trait ",
        "type ",
        "const ",
        "pub const ",
        "class ",
        "def ",
        "async def ",
        "function ",
        "interface ",
        "func ",
        "export function ",
        "export class ",
        "export const ",
    ];
    let mut symbols = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(rest) = prefixes
            .iter()
            .find_map(|prefix| trimmed.strip_prefix(prefix))
        else {
            continue;
        };
        let name = rest
            .split(|character: char| {
                character.is_whitespace() || matches!(character, '(' | '<' | '{' | ':' | '=' | ';')
            })
            .next()
            .unwrap_or_default()
            .trim_matches(|character: char| !character.is_alphanumeric() && character != '_');
        if !name.is_empty()
            && name.len() <= MAX_TOKEN_BYTES
            && !symbols.iter().any(|item| item == name)
        {
            symbols.push(name.to_owned());
            if symbols.len() >= MAX_SYMBOLS_PER_CHUNK {
                break;
            }
        }
    }
    symbols
}

fn extract_imports(text: &str) -> Vec<String> {
    let prefixes = [
        "use ", "pub use ", "mod ", "import ", "from ", "#include", "require(",
    ];
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            prefixes
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
                .then(|| sanitize_text(trimmed, 1_024))
        })
        .take(MAX_IMPORTS_PER_FILE)
        .collect()
}

fn chunk_terms(path: &str, imports: &[String], chunk: &IndexChunk) -> Vec<(String, u16)> {
    let mut combined = String::with_capacity(
        path.len()
            .saturating_add(chunk.text.len())
            .saturating_add(imports.iter().map(String::len).sum::<usize>()),
    );
    combined.push_str(path);
    combined.push('\n');
    combined.push_str(&chunk.symbols.join(" "));
    combined.push('\n');
    combined.push_str(&imports.join("\n"));
    combined.push('\n');
    combined.push_str(&chunk.text);
    let mut tokens = tokenize(&combined);
    tokens.sort_unstable();
    let mut terms: Vec<(String, u16)> = Vec::new();
    for token in tokens {
        if let Some((previous, count)) = terms.last_mut()
            && *previous == token
        {
            *count = count.saturating_add(1);
        } else {
            terms.push((token, 1_u16));
        }
    }
    terms
}

fn tokenize(value: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut previous_lowercase = false;
    for character in value.chars() {
        if character.is_alphanumeric() || character == '_' {
            if character.is_uppercase() && previous_lowercase && !current.is_empty() {
                push_token(&mut tokens, &current);
                current.clear();
            }
            if character == '_' {
                push_token(&mut tokens, &current);
                current.clear();
                previous_lowercase = false;
            } else {
                previous_lowercase = character.is_lowercase();
                if current.len() < MAX_TOKEN_BYTES {
                    current.extend(character.to_lowercase());
                }
            }
        } else {
            push_token(&mut tokens, &current);
            current.clear();
            previous_lowercase = false;
        }
    }
    push_token(&mut tokens, &current);
    tokens
}

fn push_token(tokens: &mut Vec<String>, value: &str) {
    if value.len() >= 2 {
        tokens.push(value.to_owned());
    }
}

fn repository_overview(index: &PersistedIndex, filter: Option<&str>) -> Value {
    let mut extensions = BTreeMap::<String, usize>::new();
    let mut directories = BTreeMap::<String, usize>::new();
    let mut files = 0_usize;
    let mut chunks = 0_usize;
    for file in &index.files {
        if filter.is_some_and(|filter| !path_matches_filter(&file.path, filter)) {
            continue;
        }
        files = files.saturating_add(1);
        chunks = chunks.saturating_add(file.chunks.len());
        let extension = Path::new(&file.path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("<none>")
            .to_ascii_lowercase();
        *extensions.entry(extension).or_default() += 1;
        let directory = file.path.split('/').next().unwrap_or(".").to_owned();
        *directories.entry(directory).or_default() += 1;
    }
    let extensions = top_counts(extensions, 32);
    let directories = top_counts(directories, 32);
    json!({
        "path": filter,
        "files": files,
        "chunks": chunks,
        "extensions": extensions,
        "top_level_directories": directories,
        "built_at": index.built_at,
        "generation": index.generation,
    })
}

fn top_counts(values: BTreeMap<String, usize>, limit: usize) -> Vec<Value> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    values
        .into_iter()
        .take(limit)
        .map(|(name, count)| json!({ "name": name, "count": count }))
        .collect()
}

fn repository_dependencies(index: &PersistedIndex, path: &str) -> Result<Value, CodeIndexError> {
    let file = index
        .files
        .iter()
        .find(|file| file.path == path)
        .ok_or_else(|| CodeIndexError::InvalidInput(format!("indexed file {path:?} not found")))?;
    let stem = Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let reverse = index
        .files
        .iter()
        .filter(|candidate| candidate.path != path)
        .filter(|candidate| {
            !stem.is_empty()
                && candidate
                    .imports
                    .iter()
                    .any(|import| import.to_ascii_lowercase().contains(&stem))
        })
        .map(|candidate| candidate.path.clone())
        .take(256)
        .collect::<Vec<_>>();
    Ok(json!({
        "path": file.path,
        "imports": file.imports,
        "possible_reverse_dependencies": reverse,
        "reverse_dependencies_are_heuristic": true,
    }))
}

fn bounded_json(value: &Value, limit: usize) -> Result<String, CodeIndexError> {
    let serialized = serde_json::to_string(value).map_err(|error| {
        CodeIndexError::InvalidInput(format!("could not serialize index result: {error}"))
    })?;
    if serialized.len() <= limit {
        return Ok(serialized);
    }
    let mut preview_bytes = serialized.len().min(limit.saturating_sub(128));
    loop {
        let preview = truncate_utf8(&serialized, preview_bytes);
        let output = serde_json::to_string(&json!({
            "truncated": true,
            "preview": preview,
            "original_bytes": serialized.len(),
        }))
        .map_err(|error| CodeIndexError::InvalidInput(error.to_string()))?;
        if output.len() <= limit {
            return Ok(output);
        }
        if preview.is_empty() {
            return Err(CodeIndexError::InvalidInput(
                "code index result byte limit is too small for truncation metadata".to_owned(),
            ));
        }
        preview_bytes = preview
            .len()
            .saturating_sub(output.len().saturating_sub(limit).max(1));
    }
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

fn sanitize_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars() {
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

fn workspace_fingerprint(root: &Path, privacy_sha256: &str, config: &CodeIndexConfig) -> String {
    let canonical = root.to_string_lossy().replace('\\', "/");
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hasher.update([0]);
    hasher.update(privacy_sha256.as_bytes());
    for value in [
        config.max_files,
        config.max_file_bytes,
        config.max_source_bytes,
        config.max_chunks,
        config.chunk_lines,
        config.overlap_lines,
    ] {
        hasher.update(value.to_le_bytes());
    }
    format!("{:x}", hasher.finalize())[..24].to_owned()
}

#[must_use]
pub fn is_code_index_function(name: &str) -> bool {
    matches!(
        name,
        INDEX_STATUS_TOOL | INDEX_SEARCH_TOOL | INDEX_OVERVIEW_TOOL | INDEX_DEPENDENCIES_TOOL
    )
}

#[must_use]
pub fn code_index_function_definitions() -> Vec<FunctionToolDefinition> {
    vec![
        FunctionToolDefinition::new(
            INDEX_STATUS_TOOL,
            Some("Show local repository-index freshness, progress, limits, and generation. Read-only and local.".to_owned()),
            json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false }),
        ),
        FunctionToolDefinition::new(
            INDEX_SEARCH_TOOL,
            Some("Search the local incremental code index before broad file reads. Hybrid ranking boosts identifiers, symbols, paths, imports, and exact phrases; path may be null.".to_owned()),
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES },
                    "path": { "type": ["string", "null"], "maxLength": MAX_PATH_FILTER_BYTES },
                    "top": { "type": "integer", "minimum": 1, "maximum": MAX_SEARCH_RESULTS }
                },
                "required": ["query", "path", "top"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            INDEX_OVERVIEW_TOOL,
            Some("Return a compact indexed repository or subtree overview without reading source files; path may be null.".to_owned()),
            json!({
                "type": "object",
                "properties": { "path": { "type": ["string", "null"], "maxLength": MAX_PATH_FILTER_BYTES } },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            INDEX_DEPENDENCIES_TOOL,
            Some("Return imports and bounded heuristic reverse dependencies for one indexed workspace file.".to_owned()),
            json!({
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1, "maxLength": MAX_PATH_FILTER_BYTES } },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use secrecy::SecretString;
    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method, path},
    };

    use super::{
        CodeIndexConfig, CodeIndexError, CodeIndexManager, CodeIndexState, EmbeddingConfig,
        INDEX_SEARCH_TOOL, bounded_json, code_index_function_definitions, extract_symbols,
        tokenize,
    };
    use crate::{
        config::{ApiAuth, ApiProvider},
        notice::UiNotice,
    };

    fn config() -> CodeIndexConfig {
        CodeIndexConfig {
            enabled: true,
            auto_refresh: false,
            max_files: 100,
            max_file_bytes: 64 * 1024,
            max_source_bytes: 1024 * 1024,
            max_chunks: 1_000,
            chunk_lines: 20,
            overlap_lines: 2,
            max_result_bytes: 64 * 1024,
            embeddings: EmbeddingConfig::default(),
        }
    }

    async fn wait_until_finished(manager: &mut CodeIndexManager) {
        for _ in 0..100 {
            manager.poll().await;
            if manager.snapshot().state != CodeIndexState::Building {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn wait_until_all_indexing_finished(manager: &mut CodeIndexManager) {
        for _ in 0..300 {
            manager.poll().await;
            if manager.build.is_none() && manager.embedding_build.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn tokenizer_splits_identifiers_and_symbol_extraction_is_bounded() {
        let tokens = tokenize("Authentication login_handler HTTPServer");
        assert!(tokens.iter().any(|token| token == "authentication"));
        assert!(tokens.iter().any(|token| token == "login"));
        assert!(tokens.iter().any(|token| token == "handler"));
        assert!(tokens.iter().any(|token| token == "httpserver"));
        let symbols = extract_symbols("pub async fn verify_token() {}\nclass SessionStore:\n");
        assert_eq!(symbols, ["verify_token", "SessionStore"]);
    }

    #[tokio::test]
    async fn remote_embeddings_build_a_real_hybrid_index_with_azure_auth()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/embeddings"))
            .and(header("api-key", "secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"index": 0, "embedding": [3.0, 4.0, 0.0]}]
            })))
            .expect(2)
            .mount(&server)
            .await;
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::create_dir_all(root.path().join("src"))?;
        fs::write(
            root.path().join("src/auth.rs"),
            "pub fn validate_session_token() -> bool { true }\n",
        )?;
        let mut settings = config();
        settings.embeddings = EmbeddingConfig {
            enabled: true,
            endpoint: format!("{}/embeddings", server.uri()),
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
            hybrid_weight: 0.7,
        };
        let mut manager = CodeIndexManager::new(settings, root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_all_indexing_finished(&mut manager).await;
        assert_eq!(manager.snapshot().embedded_chunks, 1);
        assert!(matches!(
            manager.snapshot().embedding_notice,
            UiNotice::EmbeddingReady { .. }
        ));
        let hits = manager
            .search("authorization security", None, 5, &CancellationToken::new())
            .await?;
        assert_eq!(
            hits.first().map(|hit| hit.path.as_str()),
            Some("src/auth.rs")
        );
        assert!((0.0..=1.0).contains(&hits[0].score));
        Ok(())
    }

    #[tokio::test]
    async fn incremental_refresh_reuses_unchanged_files_and_updates_changed_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(
            root.path().join("auth.rs"),
            "pub fn verify_token() { /* authentication session */ }\n",
        )?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start().await;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().state, CodeIndexState::Ready);
        assert_eq!(manager.snapshot().changed_files, 1);

        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().reused_files, 1);
        assert_eq!(manager.snapshot().changed_files, 0);

        fs::write(
            root.path().join("auth.rs"),
            "pub fn rotate_token() { /* authentication session */ }\n",
        )?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().changed_files, 1);
        Ok(())
    }

    #[tokio::test]
    async fn search_is_ranked_bounded_and_survives_cache_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::create_dir(root.path().join("src"))?;
        fs::write(
            root.path().join("src/auth.rs"),
            "pub fn login_handler() { verify_token(); }\nfn verify_token() {}\n",
        )?;
        fs::write(
            root.path().join("src/math.rs"),
            "pub fn add(a:i32,b:i32)->i32{a+b}\n",
        )?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        let output = manager
            .call(
                INDEX_SEARCH_TOOL,
                r#"{"query":"login token","path":null,"top":5}"#,
                &CancellationToken::new(),
            )
            .await?;
        assert!(output.contains("src/auth.rs"));
        assert!(!output.contains("src/math.rs"));

        let mut reloaded = CodeIndexManager::new(config(), root.path(), storage.path())?;
        reloaded.start().await;
        assert_eq!(reloaded.snapshot().state, CodeIndexState::Ready);
        let output = reloaded
            .call(
                INDEX_SEARCH_TOOL,
                r#"{"query":"verify_token","path":"src","top":1}"#,
                &CancellationToken::new(),
            )
            .await?;
        assert!(output.contains("verify_token"));
        Ok(())
    }

    #[tokio::test]
    async fn changed_chunking_config_does_not_load_a_stale_cache()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("main.rs"), "fn main() {}\n")?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().state, CodeIndexState::Ready);

        let mut changed = config();
        changed.chunk_lines = 21;
        let mut reloaded = CodeIndexManager::new(changed, root.path(), storage.path())?;
        reloaded.start().await;
        assert_eq!(reloaded.snapshot().state, CodeIndexState::Empty);
        Ok(())
    }

    #[tokio::test]
    async fn one_character_query_can_find_an_exact_match() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("single.rs"), "fn x() {}\n")?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;

        let hits = manager
            .search("x", None, 5, &CancellationToken::new())
            .await?;
        assert_eq!(hits.first().map(|hit| hit.path.as_str()), Some("single.rs"));
        Ok(())
    }

    #[tokio::test]
    async fn path_filter_matches_path_components_not_prefixes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::create_dir(root.path().join("src"))?;
        fs::create_dir(root.path().join("src2"))?;
        fs::write(root.path().join("src/inside.rs"), "fn shared_marker() {}\n")?;
        fs::write(
            root.path().join("src2/outside.rs"),
            "fn shared_marker() {}\n",
        )?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;

        let hits = manager
            .search("shared_marker", Some("src"), 10, &CancellationToken::new())
            .await?;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "src/inside.rs");
        Ok(())
    }

    #[tokio::test]
    async fn chunk_limit_reports_unprocessed_files_as_skipped()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("a.rs"), "fn first() {}\n")?;
        fs::write(root.path().join("b.rs"), "fn second() {}\n")?;
        let mut settings = config();
        settings.max_chunks = 1;
        let mut manager = CodeIndexManager::new(settings, root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;

        let snapshot = manager.snapshot();
        assert_eq!(snapshot.total_files, 2);
        assert_eq!(snapshot.scanned_files, 2);
        assert_eq!(snapshot.indexed_files, 1);
        assert_eq!(snapshot.skipped_files, 1);
        Ok(())
    }

    #[test]
    fn bounded_json_accounts_for_escape_expansion() -> Result<(), Box<dyn std::error::Error>> {
        let output = bounded_json(&serde_json::json!({"value": "\\\"".repeat(10_000)}), 4_096)?;
        assert!(output.len() <= 4_096);
        assert!(output.contains("\"truncated\":true"));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_retains_previous_ready_generation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("main.rs"), "fn main() {}\n")?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        let generation = manager.snapshot().generation;
        manager.start_refresh(true)?;
        manager.cancel_refresh()?;
        wait_until_finished(&mut manager).await;
        assert!(matches!(
            manager.snapshot().state,
            CodeIndexState::Cancelled | CodeIndexState::Ready
        ));
        assert_eq!(manager.snapshot().generation, generation);
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_cache_is_nonfatal_and_can_be_rebuilt() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("main.rs"), "fn main() {}\n")?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        let cache_parent = manager.cache_path.parent().ok_or("cache parent")?;
        fs::create_dir_all(cache_parent)?;
        fs::write(&manager.cache_path, b"{torn")?;
        manager.start().await;
        assert_eq!(manager.snapshot().state, CodeIndexState::Error);

        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().state, CodeIndexState::Ready);
        assert_eq!(manager.snapshot().indexed_files, 1);
        Ok(())
    }

    #[tokio::test]
    async fn gitignore_binary_and_terminal_controls_never_pollute_results()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join(".gitignore"), "ignored.rs\n")?;
        fs::write(
            root.path().join("ignored.rs"),
            "fn ignored_secret_marker() {}\n",
        )?;
        fs::write(root.path().join("binary.dat"), [0_u8, 1, 2, 3])?;
        fs::write(
            root.path().join("visible.rs"),
            "fn visible_marker() { /* bad\u{1b}[31m\u{202e} */ }\n",
        )?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(manager.snapshot().indexed_files, 1);
        let ignored = manager
            .search("ignored_secret", None, 5, &CancellationToken::new())
            .await?;
        assert!(ignored.is_empty());
        let visible = manager
            .search("visible_marker", None, 5, &CancellationToken::new())
            .await?;
        assert_eq!(visible.len(), 1);
        assert!(!visible[0].snippet.contains('\u{1b}'));
        assert!(!visible[0].snippet.contains('\u{202e}'));
        Ok(())
    }

    #[tokio::test]
    async fn privacy_reload_invalidates_cache_and_excludes_newly_blocked_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(
            root.path().join("private.rs"),
            "fn unique_private_symbol() {}\n",
        )?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert_eq!(
            manager
                .search("unique_private_symbol", None, 5, &CancellationToken::new())
                .await?
                .len(),
            1
        );

        fs::write(root.path().join(".decodeignore"), "private.rs\n")?;
        let privacy = manager.privacy.as_ref().ok_or("privacy handle")?;
        privacy.reload()?;
        assert!(manager.privacy_reloaded()?);
        assert!(matches!(
            manager
                .search("unique_private_symbol", None, 5, &CancellationToken::new())
                .await,
            Err(CodeIndexError::NotReady(_))
        ));
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;
        assert!(
            manager
                .search("unique_private_symbol", None, 5, &CancellationToken::new())
                .await?
                .is_empty()
        );
        Ok(())
    }

    #[tokio::test]
    async fn privacy_reload_discards_a_completed_unpolled_build()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let storage = tempdir()?;
        fs::write(root.path().join("private.rs"), "fn private_marker() {}\n")?;
        let mut manager = CodeIndexManager::new(config(), root.path(), storage.path())?;
        manager.start_refresh(false)?;
        wait_until_finished(&mut manager).await;

        manager.start_refresh(false)?;
        for _ in 0..100 {
            if manager
                .build
                .as_ref()
                .is_some_and(|build| build.handle.is_finished())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            manager
                .build
                .as_ref()
                .is_some_and(|build| build.handle.is_finished())
        );

        fs::write(root.path().join(".decodeignore"), "private.rs\n")?;
        manager.privacy.as_ref().ok_or("privacy handle")?.reload()?;
        assert!(manager.privacy_reloaded()?);
        manager.poll().await;

        assert!(matches!(
            manager
                .search("private_marker", None, 5, &CancellationToken::new())
                .await,
            Err(CodeIndexError::NotReady(_))
        ));
        Ok(())
    }

    #[test]
    fn strict_tools_and_unsafe_filters_fail_closed() -> Result<(), serde_json::Error> {
        let definitions = code_index_function_definitions();
        assert_eq!(definitions.len(), 4);
        let value = serde_json::to_value(definitions)?;
        assert!(value.as_array().is_some_and(|definitions| {
            definitions.iter().all(|definition| {
                definition["strict"] == true
                    && definition["parameters"]["additionalProperties"] == false
            })
        }));
        assert!(matches!(
            super::validate_relative_filter("../secret"),
            Err(CodeIndexError::InvalidInput(_))
        ));
        assert!(matches!(
            super::validate_relative_filter(&"a".repeat(16 * 1024 + 1)),
            Err(CodeIndexError::InvalidInput(_))
        ));
        Ok(())
    }
}
