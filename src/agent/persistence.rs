use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use fs2::FileExt as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use super::state::{AgentState, HistoryKind};

const SESSION_FORMAT_VERSION: u32 = 1;
const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SESSION_LINE_BYTES: usize = 8 * 1024 * 1024;
const MAX_SESSION_JOURNAL_BYTES: u64 = MAX_SESSION_FILE_BYTES + MAX_SESSION_LINE_BYTES as u64 + 1;
const MAX_SESSION_FILES: usize = 10_000;
const MAX_TITLE_BYTES: usize = 512;
const TITLE_GRAPHEMES: usize = 96;
const PREVIEW_GRAPHEMES: usize = 160;

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("session I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("session JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session ID is invalid: {0}")]
    InvalidId(String),
    #[error("session {0} was not found")]
    NotFound(String),
    #[error("session title must contain visible text and be at most {MAX_TITLE_BYTES} bytes")]
    InvalidTitle,
    #[error("session {id} belongs to {saved:?}, not the current workspace {current:?}")]
    WorkspaceMismatch {
        id: String,
        saved: PathBuf,
        current: PathBuf,
    },
    #[error("session journal has no valid header")]
    MissingHeader,
    #[error("session journal has no valid state snapshot")]
    MissingSnapshot,
    #[error("session journal record checksum does not match")]
    ChecksumMismatch,
    #[error("session journal line exceeds {MAX_SESSION_LINE_BYTES} bytes")]
    LineTooLarge,
    #[error("session journal file exceeds {MAX_SESSION_JOURNAL_BYTES} bytes")]
    FileTooLarge,
    #[error("session directory contains more than {MAX_SESSION_FILES} journals")]
    TooManySessions,
    #[error("blocking session worker failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(String);

impl SessionId {
    fn new() -> Self {
        Self(format!(
            "{:x}-{:016x}",
            Utc::now().timestamp_micros(),
            fastrand::u64(..)
        ))
    }

    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, SessionError> {
        let value = value.into();
        let valid = !value.is_empty()
            && value.len() <= 96
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
        if valid {
            Ok(Self(value))
        } else {
            Err(SessionError::InvalidId(value))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub preview: String,
    pub workspace_root: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub pinned: bool,
    pub archived: bool,
    pub history_entries: usize,
    pub parent_session_id: Option<SessionId>,
    pub recovered_records: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionDocument {
    pub summary: SessionSummary,
    pub state: AgentState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionHeader {
    format_version: u32,
    id: SessionId,
    workspace_root: PathBuf,
    created_at: DateTime<Utc>,
    parent_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMetadata {
    title: String,
    pinned: bool,
    archived: bool,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionEvent {
    Header {
        header: SessionHeader,
        metadata: SessionMetadata,
    },
    Snapshot {
        saved_at: DateTime<Utc>,
        state: Box<AgentState>,
    },
    Metadata {
        metadata: SessionMetadata,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct JournalEnvelope {
    version: u32,
    sha256: String,
    payload: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    directory: PathBuf,
    workspace_root: PathBuf,
    max_file_bytes: u64,
}

impl SessionStore {
    pub(crate) fn open_at(directory: PathBuf, workspace_root: &Path) -> Result<Self, SessionError> {
        let workspace_root =
            fs::canonicalize(workspace_root).map_err(|source| SessionError::Io {
                path: workspace_root.to_path_buf(),
                source,
            })?;
        fs::create_dir_all(&directory).map_err(|source| SessionError::Io {
            path: directory.clone(),
            source,
        })?;
        Ok(Self {
            directory,
            workspace_root,
            max_file_bytes: MAX_SESSION_FILE_BYTES,
        })
    }

    #[cfg(test)]
    fn with_max_file_bytes(mut self, max_file_bytes: u64) -> Self {
        self.max_file_bytes = max_file_bytes;
        self
    }

    pub(crate) async fn create(
        &self,
        title_seed: String,
        state: AgentState,
        parent_session_id: Option<SessionId>,
    ) -> Result<SessionDocument, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.create_sync(&title_seed, &state, parent_session_id)
        })
        .await?
    }

    pub(crate) async fn save(
        &self,
        id: SessionId,
        state: AgentState,
    ) -> Result<SessionSummary, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.save_sync(&id, &state)).await?
    }

    pub(crate) async fn load(
        &self,
        id: SessionId,
        allow_workspace_mismatch: bool,
    ) -> Result<SessionDocument, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut document =
                store.with_session_lock(&id, |path| load_document(path, &id, true))?;
            if !allow_workspace_mismatch
                && !same_workspace(&document.summary.workspace_root, &store.workspace_root)
            {
                return Err(SessionError::WorkspaceMismatch {
                    id: id.to_string(),
                    saved: document.summary.workspace_root,
                    current: store.workspace_root,
                });
            }
            document.state.recover_after_restart();
            document.summary.history_entries = document.state.visible_history().len();
            Ok(document)
        })
        .await?
    }

    pub(crate) async fn list(
        &self,
        query: String,
        include_archived: bool,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.list_sync(&query, include_archived)).await?
    }

    pub(crate) async fn rename(
        &self,
        id: SessionId,
        title: String,
    ) -> Result<SessionSummary, SessionError> {
        let title = validate_title(&title)?;
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.update_metadata_sync(&id, |metadata| metadata.title = title)
        })
        .await?
    }

    pub(crate) async fn set_pinned(
        &self,
        id: SessionId,
        pinned: bool,
    ) -> Result<SessionSummary, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.update_metadata_sync(&id, |metadata| metadata.pinned = pinned)
        })
        .await?
    }

    pub(crate) async fn set_archived(
        &self,
        id: SessionId,
        archived: bool,
    ) -> Result<SessionSummary, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store.update_metadata_sync(&id, |metadata| metadata.archived = archived)
        })
        .await?
    }

    pub(crate) async fn fork(&self, source: SessionId) -> Result<SessionDocument, SessionError> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            let source_document =
                store.with_session_lock(&source, |path| load_document(path, &source, true))?;
            let title = format!("{} (fork)", source_document.summary.title);
            let mut state = source_document.state;
            state.recover_after_restart();
            // A fork inherits context but starts a new billing ledger: the
            // parent tokens were not charged again when its JSONL was copied.
            state.reset_billing_usage();
            // Side questions are intentionally outside the main causal
            // context, so a fork starts with a clean side transcript too.
            state.side_chat.clear();
            // Pending controls must never execute a second time in a fork.
            state.follow_ups.clear();
            store.create_sync(&title, &state, Some(source))
        })
        .await?
    }

    fn create_sync(
        &self,
        title_seed: &str,
        state: &AgentState,
        parent_session_id: Option<SessionId>,
    ) -> Result<SessionDocument, SessionError> {
        let id = SessionId::new();
        let now = Utc::now();
        let title = title_from_seed(title_seed);
        let header = SessionHeader {
            format_version: SESSION_FORMAT_VERSION,
            id: id.clone(),
            workspace_root: self.workspace_root.clone(),
            created_at: now,
            parent_session_id,
        };
        let metadata = SessionMetadata {
            title,
            pinned: false,
            archived: false,
            updated_at: now,
        };
        self.with_session_lock(&id, |path| {
            if path.exists() {
                return Err(SessionError::InvalidId(id.to_string()));
            }
            rewrite_journal(
                path,
                &[
                    SessionEvent::Header {
                        header: header.clone(),
                        metadata: metadata.clone(),
                    },
                    SessionEvent::Snapshot {
                        saved_at: now,
                        state: Box::new(state.clone()),
                    },
                ],
            )
        })?;
        Ok(document_from_parts(header, metadata, state.clone(), now, 0))
    }

    fn save_sync(
        &self,
        id: &SessionId,
        state: &AgentState,
    ) -> Result<SessionSummary, SessionError> {
        self.with_session_lock(id, |path| {
            let document = load_document(path, id, true)?;
            let saved_at = Utc::now();
            append_event(
                path,
                &SessionEvent::Snapshot {
                    saved_at,
                    state: Box::new(state.clone()),
                },
            )?;
            let updated = document_from_parts(
                SessionHeader {
                    format_version: SESSION_FORMAT_VERSION,
                    id: document.summary.id.clone(),
                    workspace_root: document.summary.workspace_root.clone(),
                    created_at: document.summary.created_at,
                    parent_session_id: document.summary.parent_session_id.clone(),
                },
                SessionMetadata {
                    title: document.summary.title,
                    pinned: document.summary.pinned,
                    archived: document.summary.archived,
                    updated_at: saved_at,
                },
                state.clone(),
                saved_at,
                document.summary.recovered_records,
            );
            compact_if_needed(path, &updated, self.max_file_bytes)?;
            Ok(updated.summary)
        })
    }

    fn update_metadata_sync<F>(
        &self,
        id: &SessionId,
        update: F,
    ) -> Result<SessionSummary, SessionError>
    where
        F: FnOnce(&mut SessionMetadata),
    {
        self.with_session_lock(id, |path| {
            let document = load_document(path, id, true)?;
            let mut metadata = SessionMetadata {
                title: document.summary.title,
                pinned: document.summary.pinned,
                archived: document.summary.archived,
                updated_at: Utc::now(),
            };
            update(&mut metadata);
            append_event(
                path,
                &SessionEvent::Metadata {
                    metadata: metadata.clone(),
                },
            )?;
            let updated = document_from_parts(
                SessionHeader {
                    format_version: SESSION_FORMAT_VERSION,
                    id: document.summary.id,
                    workspace_root: document.summary.workspace_root,
                    created_at: document.summary.created_at,
                    parent_session_id: document.summary.parent_session_id,
                },
                metadata,
                document.state,
                document.summary.updated_at,
                document.summary.recovered_records,
            );
            compact_if_needed(path, &updated, self.max_file_bytes)?;
            Ok(updated.summary)
        })
    }

    fn list_sync(
        &self,
        query: &str,
        include_archived: bool,
    ) -> Result<Vec<SessionSummary>, SessionError> {
        let mut paths = fs::read_dir(&self.directory)
            .map_err(|source| SessionError::Io {
                path: self.directory.clone(),
                source,
            })?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .take(MAX_SESSION_FILES.saturating_add(1))
            .collect::<Vec<_>>();
        if paths.len() > MAX_SESSION_FILES {
            return Err(SessionError::TooManySessions);
        }
        paths.sort();
        let query = query.trim().to_lowercase();
        let mut sessions = Vec::new();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            let Ok(id) = SessionId::parse(stem) else {
                continue;
            };
            match self.with_session_lock(&id, |session_path| load_document(session_path, &id, true))
            {
                Ok(document)
                    if (include_archived || !document.summary.archived)
                        && session_matches(&document, &query) =>
                {
                    sessions.push(document.summary);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(path = ?path, %error, "ignored unreadable session journal");
                }
            }
        }
        sessions.sort_by(|left, right| {
            right
                .pinned
                .cmp(&left.pinned)
                .then_with(|| right.updated_at.cmp(&left.updated_at))
                .then_with(|| left.id.as_str().cmp(right.id.as_str()))
        });
        Ok(sessions)
    }

    fn with_session_lock<T, F>(&self, id: &SessionId, operation: F) -> Result<T, SessionError>
    where
        F: FnOnce(&Path) -> Result<T, SessionError>,
    {
        let journal_path = self.session_path(id)?;
        let lock_path = self.directory.join(format!("{}.lock", id.as_str()));
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| SessionError::Io {
                path: lock_path.clone(),
                source,
            })?;
        lock.lock_exclusive().map_err(|source| SessionError::Io {
            path: lock_path.clone(),
            source,
        })?;
        let result = operation(&journal_path);
        if let Err(source) = fs2::FileExt::unlock(&lock) {
            tracing::warn!(path = ?lock_path, %source, "could not unlock session journal");
        }
        result
    }

    fn session_path(&self, id: &SessionId) -> Result<PathBuf, SessionError> {
        let checked = SessionId::parse(id.as_str())?;
        Ok(self.directory.join(format!("{}.jsonl", checked.as_str())))
    }
}

fn load_document(
    path: &Path,
    expected_id: &SessionId,
    repair_tail: bool,
) -> Result<SessionDocument, SessionError> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(repair_tail)
        .open(path)
        .map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                SessionError::NotFound(path.display().to_string())
            } else {
                SessionError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
    let file_size = file.metadata().map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if file_size.len() > MAX_SESSION_JOURNAL_BYTES {
        return Err(SessionError::FileTooLarge);
    }
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(MAX_SESSION_JOURNAL_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(bytes.len()).map_or(true, |length| length > MAX_SESSION_JOURNAL_BYTES) {
        return Err(SessionError::FileTooLarge);
    }

    let mut header = None;
    let mut metadata = None;
    let mut state = None;
    let mut latest_snapshot_at = None;
    let mut recovered_records = 0_usize;
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        let relative_end = bytes[cursor..].iter().position(|byte| *byte == b'\n');
        let (line_end, complete) = relative_end.map_or((bytes.len(), false), |offset| {
            (cursor.saturating_add(offset), true)
        });
        let line = &bytes[cursor..line_end];
        if line.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionError::LineTooLarge);
        }
        let is_last = !complete || line_end.saturating_add(1) >= bytes.len();
        match decode_event(line) {
            Ok(SessionEvent::Header {
                header: candidate,
                metadata: initial_metadata,
            }) if header.is_none()
                && candidate.format_version == SESSION_FORMAT_VERSION
                && candidate.id.as_str() == expected_id.as_str() =>
            {
                header = Some(candidate);
                metadata = Some(initial_metadata);
            }
            Ok(SessionEvent::Snapshot {
                saved_at,
                state: candidate,
            }) if header.is_some() => {
                let mut candidate = *candidate;
                if let Some(previous) = state.as_ref() {
                    candidate.recover_legacy_visible_history(previous);
                }
                state = Some(candidate);
                latest_snapshot_at = Some(saved_at);
            }
            Ok(SessionEvent::Metadata {
                metadata: candidate,
            }) if header.is_some() => metadata = Some(candidate),
            Ok(_) | Err(_) => {
                recovered_records = recovered_records.saturating_add(1);
                if is_last && repair_tail {
                    file.set_len(u64::try_from(cursor).unwrap_or(0))
                        .map_err(|source| SessionError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    file.seek(SeekFrom::End(0))
                        .map_err(|source| SessionError::Io {
                            path: path.to_path_buf(),
                            source,
                        })?;
                    file.sync_data().map_err(|source| SessionError::Io {
                        path: path.to_path_buf(),
                        source,
                    })?;
                    break;
                }
            }
        }
        cursor = if complete {
            line_end.saturating_add(1)
        } else {
            line_end
        };
    }

    let header = header.ok_or(SessionError::MissingHeader)?;
    let metadata = metadata.ok_or(SessionError::MissingHeader)?;
    let state = state.ok_or(SessionError::MissingSnapshot)?;
    let saved_at = latest_snapshot_at.unwrap_or(header.created_at);
    Ok(document_from_parts(
        header,
        metadata,
        state,
        saved_at,
        recovered_records,
    ))
}

fn document_from_parts(
    header: SessionHeader,
    metadata: SessionMetadata,
    state: AgentState,
    latest_snapshot_at: DateTime<Utc>,
    recovered_records: usize,
) -> SessionDocument {
    let updated_at = metadata.updated_at.max(latest_snapshot_at);
    let preview = history_preview(&state);
    let history_entries = state.visible_history().len();
    SessionDocument {
        summary: SessionSummary {
            id: header.id,
            title: metadata.title,
            preview,
            workspace_root: header.workspace_root,
            created_at: header.created_at,
            updated_at,
            pinned: metadata.pinned,
            archived: metadata.archived,
            history_entries,
            parent_session_id: header.parent_session_id,
            recovered_records,
        },
        state,
    }
}

fn append_event(path: &Path, event: &SessionEvent) -> Result<(), SessionError> {
    let line = encode_event(event)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let length = file
        .metadata()
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if length > 0 {
        file.seek(SeekFrom::End(-1))
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let mut last = [0_u8; 1];
        file.read_exact(&mut last)
            .map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if last[0] != b'\n' {
            file.write_all(b"\n").map_err(|source| SessionError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        }
    }
    file.write_all(&line).map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.write_all(b"\n").map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.flush().map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_data().map_err(|source| SessionError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn compact_if_needed(
    path: &Path,
    document: &SessionDocument,
    max_file_bytes: u64,
) -> Result<(), SessionError> {
    let size = fs::metadata(path)
        .map_err(|source| SessionError::Io {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    if size <= max_file_bytes {
        return Ok(());
    }
    let header = SessionHeader {
        format_version: SESSION_FORMAT_VERSION,
        id: document.summary.id.clone(),
        workspace_root: document.summary.workspace_root.clone(),
        created_at: document.summary.created_at,
        parent_session_id: document.summary.parent_session_id.clone(),
    };
    let metadata = SessionMetadata {
        title: document.summary.title.clone(),
        pinned: document.summary.pinned,
        archived: document.summary.archived,
        updated_at: document.summary.updated_at,
    };
    rewrite_journal(
        path,
        &[
            SessionEvent::Header { header, metadata },
            SessionEvent::Snapshot {
                saved_at: document.summary.updated_at,
                state: Box::new(document.state.clone()),
            },
        ],
    )
}

fn rewrite_journal(path: &Path, events: &[SessionEvent]) -> Result<(), SessionError> {
    let parent = path.parent().ok_or_else(|| SessionError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path has no parent",
        ),
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| SessionError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    for event in events {
        let line = encode_event(event)?;
        temporary
            .write_all(&line)
            .and_then(|()| temporary.write_all(b"\n"))
            .map_err(|source| SessionError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
    }
    temporary.flush().map_err(|source| SessionError::Io {
        path: temporary.path().to_path_buf(),
        source,
    })?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| SessionError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary.persist(path).map_err(|error| SessionError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn encode_event(event: &SessionEvent) -> Result<Vec<u8>, SessionError> {
    let payload = serde_json::to_string(event)?;
    if payload.len() > MAX_SESSION_LINE_BYTES {
        return Err(SessionError::LineTooLarge);
    }
    let envelope = JournalEnvelope {
        version: SESSION_FORMAT_VERSION,
        sha256: sha256_hex(payload.as_bytes()),
        payload,
    };
    let encoded = serde_json::to_vec(&envelope)?;
    if encoded.len() > MAX_SESSION_LINE_BYTES {
        return Err(SessionError::LineTooLarge);
    }
    Ok(encoded)
}

fn decode_event(line: &[u8]) -> Result<SessionEvent, SessionError> {
    let envelope: JournalEnvelope = serde_json::from_slice(line)?;
    if envelope.version != SESSION_FORMAT_VERSION
        || sha256_hex(envelope.payload.as_bytes()) != envelope.sha256
    {
        return Err(SessionError::ChecksumMismatch);
    }
    let event = serde_json::from_str(&envelope.payload)?;
    validate_persisted_event(&event)?;
    Ok(event)
}

fn validate_persisted_event(event: &SessionEvent) -> Result<(), SessionError> {
    let metadata = match event {
        SessionEvent::Header { metadata, .. } | SessionEvent::Metadata { metadata } => metadata,
        SessionEvent::Snapshot { .. } => return Ok(()),
    };
    let normalized = validate_title(&metadata.title)?;
    if normalized != metadata.title {
        return Err(SessionError::InvalidTitle);
    }
    Ok(())
}

fn validate_title(value: &str) -> Result<String, SessionError> {
    let value = value.trim();
    if value.is_empty()
        || value.width() == 0
        || value.len() > MAX_TITLE_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(SessionError::InvalidTitle);
    }
    Ok(value.graphemes(true).take(TITLE_GRAPHEMES).collect())
}

fn title_from_seed(seed: &str) -> String {
    let compact = seed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New session")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .graphemes(true)
        .take(TITLE_GRAPHEMES)
        .collect::<String>();
    validate_title(&compact).unwrap_or_else(|_| "New session".to_owned())
}

fn history_preview(state: &AgentState) -> String {
    state
        .visible_history()
        .into_iter()
        .find_map(|entry| {
            matches!(entry.kind, HistoryKind::User).then(|| {
                entry
                    .content
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .graphemes(true)
                    .take(PREVIEW_GRAPHEMES)
                    .collect::<String>()
            })
        })
        .unwrap_or_default()
}

fn session_matches(document: &SessionDocument, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let summary = &document.summary;
    summary.title.to_lowercase().contains(query)
        || summary.preview.to_lowercase().contains(query)
        || summary.id.as_str().contains(query)
        || summary
            .workspace_root
            .to_string_lossy()
            .to_lowercase()
            .contains(query)
        || document
            .state
            .visible_history()
            .into_iter()
            .any(|entry| entry.content.to_lowercase().contains(query))
}

fn same_workspace(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write as _};

    use tempfile::TempDir;

    use super::{
        MAX_SESSION_FILE_BYTES, MAX_SESSION_LINE_BYTES, SessionError, SessionEvent, SessionId,
        SessionMetadata, SessionStore, encode_event,
    };
    use crate::{
        agent::{FollowUpMode, FollowUpStatus, state::AgentState},
        api::ReasoningEffort,
        parser::ToolOutcome,
    };

    fn store() -> Result<(TempDir, TempDir, SessionStore), Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let data = TempDir::new()?;
        let store = SessionStore::open_at(data.path().to_path_buf(), workspace.path())?;
        Ok((workspace, data, store))
    }

    #[tokio::test]
    async fn truncated_tail_is_repaired_and_session_remains_appendable()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "persistent task");
        let created = store
            .create("persistent task".to_owned(), state.clone(), None)
            .await?;
        let path = data
            .path()
            .join(format!("{}.jsonl", created.summary.id.as_str()));
        let good_len = fs::metadata(&path)?.len();
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        file.write_all(b"{\"version\":1,\"sha256\":\"cut")?;
        drop(file);

        let loaded = store.load(created.summary.id.clone(), false).await?;
        assert_eq!(loaded.state.history.len(), 1);
        assert_eq!(fs::metadata(&path)?.len(), good_len);
        state.push_user(2, "after recovery");
        store.save(created.summary.id.clone(), state).await?;
        let reloaded = store.load(created.summary.id, false).await?;
        assert_eq!(reloaded.state.history.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn loading_legacy_compaction_recovers_visible_history_from_earlier_snapshots()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, _data, store) = store()?;
        let mut full = AgentState::new();
        full.push_user(1, "first request");
        full.push_tool_result(
            1,
            1,
            "read_file",
            &ToolOutcome::success("full tool output".repeat(128)),
        );
        full.push_assistant(1, "first answer");
        full.push_user(2, "latest request");
        full.push_assistant(2, "latest answer");
        let created = store
            .create("legacy compaction".to_owned(), full.clone(), None)
            .await?;

        let mut compacted = full.clone();
        let mut compacted_tool = compacted.history[1].clone();
        compacted_tool.content =
            "[tool result compacted: tool=read_file, action_id=1, path=-, bytes=2048]".to_owned();
        let mut capsule = compacted.history[2].clone();
        capsule.content = "[deterministic extractive context capsule; whitespace-normalized excerpts; no generated claims; full turns remain in session history]\n- turn=1"
            .to_owned();
        compacted.history = vec![
            compacted.history[0].clone(),
            compacted_tool,
            capsule,
            compacted.history[3].clone(),
            compacted.history[4].clone(),
        ];
        compacted.push_superseded_assistant(
            0,
            "[1 older history entries compacted into deterministic API-context summaries]",
        );
        store.save(created.summary.id.clone(), compacted).await?;

        let session_id = created.summary.id;
        let loaded = store.load(session_id.clone(), false).await?;
        let visible = loaded.state.visible_history();
        assert!(visible.iter().any(|entry| entry.content == "first answer"));
        assert!(
            visible
                .iter()
                .any(|entry| entry.content.starts_with("full tool output"))
        );
        assert!(visible.iter().all(|entry| {
            !entry
                .content
                .starts_with("[deterministic extractive context capsule;")
                && !entry.content.starts_with("[tool result compacted:")
        }));
        drop(visible);
        store.save(session_id.clone(), loaded.state).await?;
        let reloaded = store.load(session_id, false).await?;
        assert!(
            reloaded
                .state
                .visible_history()
                .iter()
                .any(|entry| entry.content == "first answer")
        );
        Ok(())
    }

    #[tokio::test]
    async fn rename_pin_archive_and_search_survive_restart()
    -> Result<(), Box<dyn std::error::Error>> {
        let (workspace, data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "find the frobnicator bug");
        let created = store.create("find bug".to_owned(), state, None).await?;
        store
            .rename(created.summary.id.clone(), "Release blocker".to_owned())
            .await?;
        store.set_pinned(created.summary.id.clone(), true).await?;
        store.set_archived(created.summary.id.clone(), true).await?;

        let reopened = SessionStore::open_at(data.path().to_path_buf(), workspace.path())?;
        assert!(reopened.list("blocker".to_owned(), false).await?.is_empty());
        let sessions = reopened.list("frobnicator".to_owned(), true).await?;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Release blocker");
        assert!(sessions[0].pinned);
        assert!(sessions[0].archived);
        Ok(())
    }

    #[tokio::test]
    async fn fork_has_parent_and_independent_journal() -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, _data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "parent");
        state.record_deployment_usage("parent-model", 10, 2, 5, 15, 1);
        state.side_chat.start(
            "private branch question".to_owned(),
            state.history_revision,
            "parent-model".to_owned(),
            ReasoningEffort::Medium,
        )?;
        state
            .follow_ups
            .enqueue(FollowUpMode::Queue, "do not duplicate".to_owned(), None)?;
        let parent = store.create("parent".to_owned(), state, None).await?;
        let fork = store.fork(parent.summary.id.clone()).await?;
        assert_ne!(fork.summary.id, parent.summary.id);
        assert_eq!(fork.summary.parent_session_id, Some(parent.summary.id));
        assert_eq!(fork.state.history.len(), 1);
        assert!(fork.state.billing_usage.is_empty());
        assert!(fork.state.side_chat.is_empty());
        assert!(fork.state.follow_ups.snapshot().items.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn restart_never_blindly_replays_pending_or_dispatching_follow_ups()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, _data, store) = store()?;
        let mut state = AgentState::new();
        let pending = state
            .follow_ups
            .enqueue(FollowUpMode::Queue, "pending".to_owned(), None)?;
        let dispatching =
            state
                .follow_ups
                .enqueue(FollowUpMode::Queue, "uncertain".to_owned(), None)?;
        state.follow_ups.begin_dispatch(dispatching.id, 44)?;
        let session = store.create("recovery".to_owned(), state, None).await?;

        let loaded = store.load(session.summary.id, false).await?;
        let pending = loaded
            .state
            .follow_ups
            .snapshot()
            .items
            .iter()
            .find(|item| item.id == pending.id)
            .cloned()
            .ok_or("pending item disappeared")?;
        let uncertain = loaded
            .state
            .follow_ups
            .snapshot()
            .items
            .iter()
            .find(|item| item.id == dispatching.id)
            .cloned()
            .ok_or("dispatching item disappeared")?;
        assert_eq!(pending.status, FollowUpStatus::Pending);
        assert!(pending.requires_manual_dispatch);
        assert_eq!(uncertain.status, FollowUpStatus::Failed);
        assert!(loaded.state.history.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn compaction_keeps_latest_state_and_metadata() -> Result<(), Box<dyn std::error::Error>>
    {
        let (workspace, data, store) = store()?;
        let store = store.with_max_file_bytes(1);
        let mut state = AgentState::new();
        state.push_user(1, "one");
        let session = store.create("one".to_owned(), state.clone(), None).await?;
        state.push_user(2, "two");
        store.save(session.summary.id.clone(), state).await?;
        store
            .rename(session.summary.id.clone(), "Compacted".to_owned())
            .await?;

        let reopened = SessionStore::open_at(data.path().to_path_buf(), workspace.path())?;
        let loaded = reopened.load(session.summary.id, false).await?;
        assert_eq!(loaded.summary.title, "Compacted");
        assert_eq!(loaded.state.history.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_middle_record_is_skipped_without_losing_newer_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "before corruption");
        let session = store
            .create("corruption".to_owned(), state.clone(), None)
            .await?;
        state.push_user(2, "newer valid state");
        store.save(session.summary.id.clone(), state).await?;

        let path = data
            .path()
            .join(format!("{}.jsonl", session.summary.id.as_str()));
        let bytes = fs::read(&path)?;
        let last_valid = bytes
            .split(|byte| *byte == b'\n')
            .rev()
            .find(|line| !line.is_empty())
            .ok_or("journal did not contain a record")?
            .to_vec();
        let mut file = fs::OpenOptions::new().append(true).open(&path)?;
        file.write_all(b"{\"broken\":true}\n")?;
        file.write_all(&last_valid)?;
        file.write_all(b"\n")?;
        drop(file);

        let loaded = store.load(session.summary.id, false).await?;
        assert_eq!(loaded.state.history.len(), 2);
        assert_eq!(loaded.summary.recovered_records, 1);
        Ok(())
    }

    #[tokio::test]
    async fn workspace_mismatch_is_fail_closed_but_can_be_explicitly_overridden()
    -> Result<(), Box<dyn std::error::Error>> {
        let (workspace, data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "workspace-bound task");
        let session = store.create("workspace".to_owned(), state, None).await?;
        let other_workspace = TempDir::new()?;
        let other = SessionStore::open_at(data.path().to_path_buf(), other_workspace.path())?;

        let denied = other.load(session.summary.id.clone(), false).await;
        assert!(matches!(
            denied,
            Err(SessionError::WorkspaceMismatch { .. })
        ));
        let allowed = other.load(session.summary.id, true).await?;
        assert_eq!(
            allowed.summary.workspace_root,
            fs::canonicalize(workspace.path())?
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_metadata_updates_are_serialized_without_lost_fields()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, _data, store) = store()?;
        let session = store
            .create("concurrent".to_owned(), AgentState::new(), None)
            .await?;
        let rename = store.rename(session.summary.id.clone(), "Durable name".to_owned());
        let pin = store.set_pinned(session.summary.id.clone(), true);
        let (renamed, pinned) = tokio::join!(rename, pin);
        renamed?;
        pinned?;

        let loaded = store.load(session.summary.id, false).await?;
        assert_eq!(loaded.summary.title, "Durable name");
        assert!(loaded.summary.pinned);
        Ok(())
    }

    #[test]
    fn session_ids_are_validated_during_deserialization() {
        let decoded = serde_json::from_str::<SessionId>(r#""../outside""#);
        assert!(decoded.is_err());
    }

    #[tokio::test]
    async fn journal_header_must_match_its_file_name() -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, data, store) = store()?;
        let session = store
            .create("original".to_owned(), AgentState::new(), None)
            .await?;
        let original = data
            .path()
            .join(format!("{}.jsonl", session.summary.id.as_str()));
        let alias = SessionId::parse("different-session")?;
        fs::copy(original, data.path().join("different-session.jsonl"))?;

        assert!(store.load(alias, false).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn snapshots_before_the_header_are_not_restored() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_workspace, data, store) = store()?;
        let mut state = AgentState::new();
        state.push_user(1, "must not be restored");
        let session = store.create("ordered".to_owned(), state, None).await?;
        let path = data
            .path()
            .join(format!("{}.jsonl", session.summary.id.as_str()));
        let lines = fs::read(&path)?
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(<[u8]>::to_vec)
            .collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        let mut reordered = lines[1].clone();
        reordered.push(b'\n');
        reordered.extend_from_slice(&lines[0]);
        reordered.push(b'\n');
        fs::write(path, reordered)?;

        assert!(matches!(
            store.load(session.summary.id, false).await,
            Err(SessionError::MissingSnapshot)
        ));
        Ok(())
    }

    #[test]
    fn encoded_records_cannot_exceed_the_reader_limit() -> Result<(), Box<dyn std::error::Error>> {
        let mut event = SessionEvent::Metadata {
            metadata: SessionMetadata {
                title: String::new(),
                pinned: false,
                archived: false,
                updated_at: chrono::Utc::now(),
            },
        };
        let overhead = serde_json::to_string(&event)?.len();
        let SessionEvent::Metadata { metadata } = &mut event else {
            return Err("test event changed variant".into());
        };
        metadata.title = "x".repeat(MAX_SESSION_LINE_BYTES.saturating_sub(overhead));

        assert!(matches!(
            encode_event(&event),
            Err(SessionError::LineTooLarge)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn titles_must_contain_visible_glyphs() -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, _data, store) = store()?;
        let session = store
            .create("visible".to_owned(), AgentState::new(), None)
            .await?;

        assert!(matches!(
            store
                .rename(session.summary.id, "\u{200b}\u{2060}".to_owned())
                .await,
            Err(SessionError::InvalidTitle)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn invalid_persisted_metadata_is_ignored() -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, data, store) = store()?;
        let session = store
            .create("safe title".to_owned(), AgentState::new(), None)
            .await?;
        let path = data
            .path()
            .join(format!("{}.jsonl", session.summary.id.as_str()));
        let line = encode_event(&SessionEvent::Metadata {
            metadata: SessionMetadata {
                title: "\u{1b}[31mspoofed".to_owned(),
                pinned: true,
                archived: true,
                updated_at: chrono::Utc::now(),
            },
        })?;
        let mut file = fs::OpenOptions::new().append(true).open(path)?;
        file.write_all(&line)?;
        file.write_all(b"\n")?;
        drop(file);

        let loaded = store.load(session.summary.id, false).await?;
        assert_eq!(loaded.summary.title, "safe title");
        assert!(!loaded.summary.pinned);
        assert!(!loaded.summary.archived);
        assert_eq!(loaded.summary.recovered_records, 1);
        Ok(())
    }

    #[tokio::test]
    async fn oversized_journals_are_rejected_before_parsing()
    -> Result<(), Box<dyn std::error::Error>> {
        let (_workspace, data, store) = store()?;
        let session = store
            .create("bounded".to_owned(), AgentState::new(), None)
            .await?;
        let path = data
            .path()
            .join(format!("{}.jsonl", session.summary.id.as_str()));
        let file = fs::OpenOptions::new().write(true).open(path)?;
        file.set_len(
            MAX_SESSION_FILE_BYTES
                .saturating_add(MAX_SESSION_LINE_BYTES as u64)
                .saturating_add(2),
        )?;

        let error = store
            .load(session.summary.id, false)
            .await
            .err()
            .ok_or("oversized journal was accepted")?;
        assert!(error.to_string().contains("journal file exceeds"));
        Ok(())
    }
}
