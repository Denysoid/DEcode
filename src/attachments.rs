use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

use crate::{
    api::types::ResponsesRequest,
    config::ApiCapabilities,
    tools::{SandboxError, SandboxRoot},
};

pub const MAX_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_ATTACHMENTS_PER_TURN: usize = 16;
pub const MAX_ATTACHMENT_TOTAL_BYTES: u64 = 50 * 1024 * 1024;
const BLOB_SCHEME: &str = "decode-attachment://";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Document,
    Text,
    Audio,
    Video,
}

impl AttachmentKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Document => "document",
            Self::Text => "text",
            Self::Audio => "audio",
            Self::Video => "video",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentDetail {
    Low,
    High,
    Original,
    #[default]
    Auto,
}

impl AttachmentDetail {
    const fn as_wire(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
            Self::Original => "original",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentRef {
    pub sha256: String,
    pub filename: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub kind: AttachmentKind,
    #[serde(default)]
    pub detail: AttachmentDetail,
}

impl AttachmentRef {
    #[must_use]
    pub fn history_label(&self) -> String {
        format!(
            "{} · {} · {}",
            self.filename,
            self.kind.label(),
            format_bytes(self.size_bytes)
        )
    }

    #[must_use]
    pub fn placeholder_part(&self) -> Value {
        let uri = format!("{BLOB_SCHEME}{}", self.sha256);
        match self.kind {
            AttachmentKind::Image => serde_json::json!({
                "type": "input_image",
                "image_url": uri,
                "detail": self.detail.as_wire(),
            }),
            AttachmentKind::Document | AttachmentKind::Text => serde_json::json!({
                "type": "input_file",
                "filename": self.filename,
                "file_data": uri,
            }),
            AttachmentKind::Audio => serde_json::json!({
                "type": "input_audio",
                "audio_data": uri,
                "format": extension(&self.filename).unwrap_or_else(|| "bin".to_owned()),
            }),
            AttachmentKind::Video => serde_json::json!({
                "type": "input_video",
                "video_data": uri,
                "mime_type": self.mime_type,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentSource {
    Workspace(String),
    /// An absolute path explicitly pasted/selected by the human in the UI.
    /// This variant is never constructed from model output.
    UserSelectedAbsolute(PathBuf),
    /// PNG bytes copied directly from the human user's native clipboard.
    /// The bytes are bounded before this value is constructed and are staged
    /// into the same content-addressed store as file-backed attachments.
    ClipboardImage {
        png_bytes: Arc<[u8]>,
        filename: String,
    },
    PastedFile {
        bytes: Arc<[u8]>,
        filename: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentDraft {
    pub source: AttachmentSource,
    pub filename: String,
    pub kind: AttachmentKind,
}

impl AttachmentDraft {
    #[must_use]
    pub fn from_workspace_path(path: impl Into<String>) -> Self {
        let workspace_path = path.into();
        let filename = Path::new(&workspace_path)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&workspace_path)
            .to_owned();
        let (kind, _) = classify(&filename);
        Self {
            source: AttachmentSource::Workspace(workspace_path),
            filename,
            kind,
        }
    }

    #[must_use]
    pub fn from_user_selected_path(path: PathBuf) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let filename = path.file_name()?.to_str()?.to_owned();
        if !is_safe_filename(&filename) {
            return None;
        }
        let (kind, mime_type) = classify(&filename);
        if mime_type == "application/octet-stream" {
            return None;
        }
        Some(Self {
            source: AttachmentSource::UserSelectedAbsolute(path),
            filename,
            kind,
        })
    }

    #[must_use]
    pub fn from_clipboard_png(png_bytes: Vec<u8>, filename: String) -> Option<Self> {
        if png_bytes.is_empty()
            || png_bytes.len() > MAX_ATTACHMENT_BYTES
            || !is_safe_filename(&filename)
        {
            return None;
        }
        Some(Self {
            source: AttachmentSource::ClipboardImage {
                png_bytes: Arc::from(png_bytes),
                filename: filename.clone(),
            },
            filename,
            kind: AttachmentKind::Image,
        })
    }

    #[must_use]
    pub fn from_pasted_bytes(bytes: Vec<u8>, filename: String) -> Option<Self> {
        if bytes.len() > MAX_ATTACHMENT_BYTES || !is_safe_filename(&filename) {
            return None;
        }
        let (kind, _) = classify(&filename);
        Some(Self {
            source: AttachmentSource::PastedFile {
                bytes: Arc::from(bytes),
                filename: filename.clone(),
            },
            filename,
            kind,
        })
    }

    pub fn snapshot_user_selected_path(path: PathBuf) -> Result<Self, AttachmentError> {
        if !path.is_absolute() {
            return Err(AttachmentError::InvalidFilename(
                path.to_string_lossy().into_owned(),
            ));
        }
        let metadata = fs::symlink_metadata(&path).map_err(|source| AttachmentError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AttachmentError::Io {
                path: path.clone(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a pasted attachment must be a regular, non-symlink file",
                ),
            });
        }
        if metadata.len() > u64::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(u64::MAX) {
            return Err(AttachmentError::TotalTooLarge);
        }
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| is_safe_filename(value))
            .ok_or_else(|| AttachmentError::InvalidFilename(path.to_string_lossy().into_owned()))?
            .to_owned();
        let mut file = fs::File::open(&path).map_err(|source| AttachmentError::Io {
            path: path.clone(),
            source,
        })?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(MAX_ATTACHMENT_BYTES)
                .min(MAX_ATTACHMENT_BYTES),
        );
        Read::by_ref(&mut file)
            .take(u64::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| AttachmentError::Io {
                path: path.clone(),
                source,
            })?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::TotalTooLarge);
        }
        Self::from_pasted_bytes(bytes, filename)
            .ok_or_else(|| AttachmentError::InvalidFilename(path.to_string_lossy().into_owned()))
    }
}

#[derive(Debug, Error)]
pub enum AttachmentError {
    #[error(transparent)]
    Sandbox(#[from] SandboxError),
    #[error("attachment store I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("attachment filename is invalid: {0:?}")]
    InvalidFilename(String),
    #[error("attachment {0:?} has an unsupported file type")]
    UnsupportedType(String),
    #[error("at most {MAX_ATTACHMENTS_PER_TURN} attachments are allowed per turn")]
    TooMany,
    #[error("attachment payload exceeds the {MAX_ATTACHMENT_TOTAL_BYTES} byte per-turn limit")]
    TotalTooLarge,
    #[error("attachment blob {0} is missing")]
    MissingBlob(String),
    #[error("attachment blob {sha256} failed its integrity check")]
    Integrity { sha256: String },
    #[error("the selected provider/model does not accept {kind} input ({filename})")]
    ProviderUnsupported {
        kind: &'static str,
        filename: String,
    },
    #[error("attachment placeholder is malformed")]
    MalformedPlaceholder,
}

#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

struct PreparedAttachment {
    reference: AttachmentRef,
    bytes: Arc<[u8]>,
}

impl AttachmentStore {
    pub fn open(root: PathBuf) -> Result<Self, AttachmentError> {
        if let Ok(metadata) = fs::symlink_metadata(&root)
            && (metadata.file_type().is_symlink() || !metadata.is_dir())
        {
            return Err(AttachmentError::Io {
                path: root,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "attachment root must be a real directory",
                ),
            });
        }
        fs::create_dir_all(&root).map_err(|source| AttachmentError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(Self { root })
    }

    pub fn stage_many(
        &self,
        sandbox: &SandboxRoot,
        requested: &[AttachmentSource],
    ) -> Result<Vec<AttachmentRef>, AttachmentError> {
        if requested.len() > MAX_ATTACHMENTS_PER_TURN {
            return Err(AttachmentError::TooMany);
        }
        let mut total = 0_u64;
        let mut prepared = Vec::with_capacity(requested.len());
        for source in requested {
            let attachment = match source {
                AttachmentSource::Workspace(path) => {
                    self.stage_one(sandbox, path, AttachmentDetail::Auto)?
                }
                AttachmentSource::UserSelectedAbsolute(path) => {
                    self.stage_user_selected(path, AttachmentDetail::Auto)?
                }
                AttachmentSource::ClipboardImage {
                    png_bytes,
                    filename,
                } => self.stage_inline_png(png_bytes, filename, AttachmentDetail::Auto)?,
                AttachmentSource::PastedFile { bytes, filename } => {
                    self.stage_inline_file(bytes, filename, AttachmentDetail::Auto)?
                }
            };
            total = total.saturating_add(attachment.reference.size_bytes);
            if total > MAX_ATTACHMENT_TOTAL_BYTES {
                return Err(AttachmentError::TotalTooLarge);
            }
            prepared.push(attachment);
        }
        for attachment in &prepared {
            self.persist_blob(&attachment.reference.sha256, &attachment.bytes)?;
        }
        Ok(prepared
            .into_iter()
            .map(|attachment| attachment.reference)
            .collect())
    }

    fn stage_inline_png(
        &self,
        png_bytes: &Arc<[u8]>,
        filename: &str,
        detail: AttachmentDetail,
    ) -> Result<PreparedAttachment, AttachmentError> {
        if png_bytes.is_empty() {
            return Err(AttachmentError::TotalTooLarge);
        }
        let prepared = self.stage_inline_file(png_bytes, filename, detail)?;
        if prepared.reference.kind != AttachmentKind::Image
            || prepared.reference.mime_type != "image/png"
        {
            return Err(AttachmentError::UnsupportedType(filename.to_owned()));
        }
        Ok(prepared)
    }

    fn stage_inline_file(
        &self,
        bytes: &Arc<[u8]>,
        filename: &str,
        detail: AttachmentDetail,
    ) -> Result<PreparedAttachment, AttachmentError> {
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::TotalTooLarge);
        }
        if !is_safe_filename(filename) {
            return Err(AttachmentError::InvalidFilename(filename.to_owned()));
        }
        let (kind, mime_type) = classify(filename);
        let sha256 = hex_digest(bytes);
        Ok(PreparedAttachment {
            reference: AttachmentRef {
                sha256,
                filename: filename.to_owned(),
                mime_type: mime_type.to_owned(),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                kind,
                detail,
            },
            bytes: Arc::clone(bytes),
        })
    }

    fn stage_user_selected(
        &self,
        requested: &Path,
        detail: AttachmentDetail,
    ) -> Result<PreparedAttachment, AttachmentError> {
        if !requested.is_absolute() {
            return Err(AttachmentError::InvalidFilename(
                requested.to_string_lossy().into_owned(),
            ));
        }
        let metadata = fs::symlink_metadata(requested).map_err(|source| AttachmentError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(AttachmentError::Io {
                path: requested.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a pasted attachment must be a regular, non-symlink file",
                ),
            });
        }
        if metadata.len() > u64::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(u64::MAX) {
            return Err(AttachmentError::TotalTooLarge);
        }
        let filename = requested
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| is_safe_filename(value))
            .ok_or_else(|| {
                AttachmentError::InvalidFilename(requested.to_string_lossy().into_owned())
            })?
            .to_owned();
        let (kind, mime_type) = classify(&filename);
        if mime_type == "application/octet-stream" {
            return Err(AttachmentError::UnsupportedType(filename));
        }
        let mut file = fs::File::open(requested).map_err(|source| AttachmentError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        let mut bytes = Vec::with_capacity(
            usize::try_from(metadata.len())
                .unwrap_or(MAX_ATTACHMENT_BYTES)
                .min(MAX_ATTACHMENT_BYTES),
        );
        Read::by_ref(&mut file)
            .take(u64::try_from(MAX_ATTACHMENT_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| AttachmentError::Io {
                path: requested.to_path_buf(),
                source,
            })?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(AttachmentError::TotalTooLarge);
        }
        let sha256 = hex_digest(&bytes);
        Ok(PreparedAttachment {
            reference: AttachmentRef {
                sha256,
                filename,
                mime_type: mime_type.to_owned(),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                kind,
                detail,
            },
            bytes: Arc::from(bytes),
        })
    }

    fn stage_one(
        &self,
        sandbox: &SandboxRoot,
        requested: &str,
        detail: AttachmentDetail,
    ) -> Result<PreparedAttachment, AttachmentError> {
        let safe = sandbox.model_file_path(requested)?;
        let bytes = sandbox.read_regular_file_limited(&safe, MAX_ATTACHMENT_BYTES)?;
        let filename = Path::new(requested)
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| is_safe_filename(value))
            .ok_or_else(|| AttachmentError::InvalidFilename(requested.to_owned()))?
            .to_owned();
        let (kind, mime_type) = classify(&filename);
        if mime_type == "application/octet-stream" {
            return Err(AttachmentError::UnsupportedType(filename));
        }
        let sha256 = hex_digest(&bytes);
        Ok(PreparedAttachment {
            reference: AttachmentRef {
                sha256,
                filename,
                mime_type: mime_type.to_owned(),
                size_bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                kind,
                detail,
            },
            bytes: Arc::from(bytes),
        })
    }

    pub fn hydrate_request(
        &self,
        request: &mut ResponsesRequest,
        capabilities: ApiCapabilities,
    ) -> Result<(), AttachmentError> {
        for item in request.input.values_mut() {
            hydrate_value(&self.root, item, capabilities)?;
        }
        Ok(())
    }

    fn persist_blob(&self, sha256: &str, bytes: &[u8]) -> Result<(), AttachmentError> {
        let target = self.root.join(sha256);
        if target.exists() {
            verify_blob(
                &target,
                sha256,
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            )?;
            return Ok(());
        }
        let mut temporary =
            NamedTempFile::new_in(&self.root).map_err(|source| AttachmentError::Io {
                path: self.root.clone(),
                source,
            })?;
        temporary
            .write_all(bytes)
            .and_then(|()| temporary.as_file_mut().sync_all())
            .map_err(|source| AttachmentError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => verify_blob(
                &target,
                sha256,
                u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            ),
            Err(error) => Err(AttachmentError::Io {
                path: target,
                source: error.error,
            }),
        }
    }
}

fn hydrate_value(
    root: &Path,
    value: &mut Value,
    capabilities: ApiCapabilities,
) -> Result<(), AttachmentError> {
    match value {
        Value::Array(items) => {
            for item in items {
                hydrate_value(root, item, capabilities)?;
            }
        }
        Value::Object(object) => {
            let item_type = object.get("type").and_then(Value::as_str);
            let field = match item_type {
                Some("input_image") => Some(("image_url", AttachmentKind::Image)),
                Some("input_file") => Some(("file_data", AttachmentKind::Document)),
                Some("input_audio") => Some(("audio_data", AttachmentKind::Audio)),
                Some("input_video") => Some(("video_data", AttachmentKind::Video)),
                _ => None,
            };
            if let Some((field, kind)) = field
                && let Some(uri) = object.get(field).and_then(Value::as_str)
                && let Some(sha256) = uri.strip_prefix(BLOB_SCHEME)
            {
                let supported = match kind {
                    AttachmentKind::Image => capabilities.images,
                    AttachmentKind::Document | AttachmentKind::Text => capabilities.files,
                    AttachmentKind::Audio => capabilities.audio,
                    AttachmentKind::Video => capabilities.video,
                };
                if !supported {
                    return Err(AttachmentError::ProviderUnsupported {
                        kind: kind.label(),
                        filename: object
                            .get("filename")
                            .and_then(Value::as_str)
                            .unwrap_or(sha256)
                            .to_owned(),
                    });
                }
                if !is_sha256(sha256) {
                    return Err(AttachmentError::MalformedPlaceholder);
                }
                let path = root.join(sha256);
                let bytes = read_verified_blob(&path, sha256)?;
                let mime = object
                    .get("mime_type")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        object
                            .get("filename")
                            .and_then(Value::as_str)
                            .map(classify)
                            .map(|(_, mime)| mime)
                    })
                    .unwrap_or(match kind {
                        AttachmentKind::Image => "image/png",
                        AttachmentKind::Audio => "audio/mpeg",
                        AttachmentKind::Video => "video/mp4",
                        AttachmentKind::Document | AttachmentKind::Text => {
                            "application/octet-stream"
                        }
                    });
                object.insert(
                    field.to_owned(),
                    Value::String(format!("data:{mime};base64,{}", STANDARD.encode(bytes))),
                );
            }
            for nested in object.values_mut() {
                hydrate_value(root, nested, capabilities)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
    Ok(())
}

fn read_verified_blob(path: &Path, sha256: &str) -> Result<Vec<u8>, AttachmentError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            AttachmentError::MissingBlob(sha256.to_owned())
        } else {
            AttachmentError::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_ATTACHMENT_BYTES as u64
    {
        return Err(AttachmentError::Integrity {
            sha256: sha256.to_owned(),
        });
    }
    let mut file = fs::File::open(path).map_err(|source| AttachmentError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.read_to_end(&mut bytes)
        .map_err(|source| AttachmentError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if hex_digest(&bytes) != sha256 {
        return Err(AttachmentError::Integrity {
            sha256: sha256.to_owned(),
        });
    }
    Ok(bytes)
}

fn verify_blob(path: &Path, sha256: &str, expected_len: u64) -> Result<(), AttachmentError> {
    let bytes = read_verified_blob(path, sha256)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != expected_len {
        return Err(AttachmentError::Integrity {
            sha256: sha256.to_owned(),
        });
    }
    Ok(())
}

fn classify(filename: &str) -> (AttachmentKind, &'static str) {
    match extension(filename).as_deref().unwrap_or_default() {
        "png" => (AttachmentKind::Image, "image/png"),
        "jpg" | "jpeg" => (AttachmentKind::Image, "image/jpeg"),
        "webp" => (AttachmentKind::Image, "image/webp"),
        "gif" => (AttachmentKind::Image, "image/gif"),
        "pdf" => (AttachmentKind::Document, "application/pdf"),
        "doc" => (AttachmentKind::Document, "application/msword"),
        "docx" => (
            AttachmentKind::Document,
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        "ppt" => (AttachmentKind::Document, "application/vnd.ms-powerpoint"),
        "pptx" => (
            AttachmentKind::Document,
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
        "xls" => (AttachmentKind::Document, "application/vnd.ms-excel"),
        "xlsx" => (
            AttachmentKind::Document,
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        "csv" => (AttachmentKind::Document, "text/csv"),
        "tsv" => (AttachmentKind::Document, "text/tab-separated-values"),
        "txt" | "md" | "markdown" | "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "go" | "java"
        | "kt" | "swift" | "c" | "cc" | "cpp" | "h" | "hpp" | "cs" | "php" | "rb" | "sh"
        | "ps1" | "toml" | "yaml" | "yml" | "json" | "xml" | "html" | "css" | "sql" | "log"
        | "diff" | "patch" => (AttachmentKind::Text, "text/plain"),
        "mp3" => (AttachmentKind::Audio, "audio/mpeg"),
        "wav" => (AttachmentKind::Audio, "audio/wav"),
        "m4a" => (AttachmentKind::Audio, "audio/mp4"),
        "ogg" | "oga" => (AttachmentKind::Audio, "audio/ogg"),
        "flac" => (AttachmentKind::Audio, "audio/flac"),
        "mp4" | "m4v" => (AttachmentKind::Video, "video/mp4"),
        "webm" => (AttachmentKind::Video, "video/webm"),
        "mov" => (AttachmentKind::Video, "video/quicktime"),
        "mkv" => (AttachmentKind::Video, "video/x-matroska"),
        _ => (AttachmentKind::Document, "application/octet-stream"),
    }
}

fn extension(filename: &str) -> Option<String> {
    Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_safe_filename(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value != "."
        && value != ".."
        && !value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    use std::fmt::Write as _;
    for byte in digest {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::privacy::PrivacyShield;

    #[test]
    fn stages_deduplicates_and_hydrates_an_image() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        fs::write(
            workspace.path().join("image.png"),
            b"not-a-real-png-but-bounded",
        )?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let attachments = store.stage_many(
            &sandbox,
            &[AttachmentSource::Workspace("image.png".to_owned())],
        )?;
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);

        let mut request = ResponsesRequest::stateless_replay(
            "model",
            "instructions",
            vec![serde_json::json!({
                "role": "user",
                "content": [attachments[0].placeholder_part()]
            })],
            128,
        );
        store.hydrate_request(&mut request, ApiCapabilities::responses_default())?;
        let value = serde_json::to_value(request)?;
        assert!(
            value["input"][0]["content"][0]["image_url"]
                .as_str()
                .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        );
        Ok(())
    }

    #[test]
    fn file_placeholder_omits_image_only_detail() {
        let reference = AttachmentRef {
            sha256: "0".repeat(64),
            filename: "notes.pdf".to_owned(),
            mime_type: "application/pdf".to_owned(),
            size_bytes: 1,
            kind: AttachmentKind::Document,
            detail: AttachmentDetail::High,
        };

        assert!(reference.placeholder_part().get("detail").is_none());
    }

    #[test]
    fn stages_an_explicit_user_selected_image_outside_the_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let selected = tempfile::tempdir()?;
        let selected_path = selected.path().join("screen.png");
        fs::write(&selected_path, b"not-real-png-but-staged-as-bytes")?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;

        let attachments = store.stage_many(
            &sandbox,
            &[AttachmentSource::UserSelectedAbsolute(
                selected_path.clone(),
            )],
        )?;

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, "screen.png");
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert!(selected_path.exists());
        Ok(())
    }

    #[test]
    fn stages_a_clipboard_png_in_the_content_addressed_store()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let png = b"\x89PNG\r\n\x1a\nclipboard-test".to_vec();

        let attachments = store.stage_many(
            &sandbox,
            &[AttachmentSource::ClipboardImage {
                png_bytes: Arc::from(png.clone()),
                filename: "clipboard-1x1.png".to_owned(),
            }],
        )?;

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].kind, AttachmentKind::Image);
        assert_eq!(attachments[0].mime_type, "image/png");
        assert_eq!(attachments[0].size_bytes, png.len() as u64);
        assert!(blobs.path().join(&attachments[0].sha256).is_file());
        Ok(())
    }

    #[test]
    fn pasted_text_reaches_the_request_without_normalization()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let original = "first\r\nЗміст 👩‍💻\nlast\0byte".as_bytes().to_vec();
        let attachments = store.stage_many(
            &sandbox,
            &[AttachmentSource::PastedFile {
                bytes: Arc::from(original.clone()),
                filename: "pasted-text.txt".to_owned(),
            }],
        )?;
        let mut request = ResponsesRequest::stateless_replay(
            "model",
            "instructions",
            vec![serde_json::json!({
                "role": "user",
                "content": [attachments[0].placeholder_part()]
            })],
            128,
        );

        store.hydrate_request(&mut request, ApiCapabilities::responses_default())?;

        let value = serde_json::to_value(request)?;
        let encoded = value["input"][0]["content"][0]["file_data"]
            .as_str()
            .and_then(|value| value.strip_prefix("data:text/plain;base64,"))
            .ok_or("missing text attachment data URL")?;
        assert_eq!(STANDARD.decode(encoded)?, original);
        Ok(())
    }

    #[test]
    fn unknown_selected_file_reaches_the_request_byte_for_byte()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let original = vec![0, 1, 2, 127, 128, 254, 255];
        let attachments = store.stage_many(
            &sandbox,
            &[AttachmentSource::PastedFile {
                bytes: Arc::from(original.clone()),
                filename: "payload.custom".to_owned(),
            }],
        )?;
        let mut request = ResponsesRequest::stateless_replay(
            "model",
            "instructions",
            vec![serde_json::json!({
                "role": "user",
                "content": [attachments[0].placeholder_part()]
            })],
            128,
        );

        store.hydrate_request(&mut request, ApiCapabilities::responses_default())?;

        let value = serde_json::to_value(request)?;
        let encoded = value["input"][0]["content"][0]["file_data"]
            .as_str()
            .and_then(|value| value.strip_prefix("data:application/octet-stream;base64,"))
            .ok_or("missing binary attachment data URL")?;
        assert_eq!(STANDARD.decode(encoded)?, original);
        Ok(())
    }

    #[test]
    fn oversized_attachment_batch_does_not_leave_blobs() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let sandbox = SandboxRoot::open_with_privacy(
            workspace.path(),
            PrivacyShield::load_project_only(workspace.path())?,
        )?;
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let mut first = vec![0_u8; 26 * 1024 * 1024];
        first[0] = 1;
        let mut second = vec![0_u8; 26 * 1024 * 1024];
        second[0] = 2;

        assert!(matches!(
            store.stage_many(
                &sandbox,
                &[
                    AttachmentSource::ClipboardImage {
                        png_bytes: Arc::from(first),
                        filename: "first.png".to_owned(),
                    },
                    AttachmentSource::ClipboardImage {
                        png_bytes: Arc::from(second),
                        filename: "second.png".to_owned(),
                    },
                ],
            ),
            Err(AttachmentError::TotalTooLarge)
        ));
        assert_eq!(fs::read_dir(blobs.path())?.count(), 0);
        Ok(())
    }

    #[test]
    fn unsupported_video_fails_before_network() -> Result<(), Box<dyn std::error::Error>> {
        let blobs = tempfile::tempdir()?;
        let store = AttachmentStore::open(blobs.path().to_path_buf())?;
        let reference = AttachmentRef {
            sha256: "0".repeat(64),
            filename: "clip.mp4".to_owned(),
            mime_type: "video/mp4".to_owned(),
            size_bytes: 1,
            kind: AttachmentKind::Video,
            detail: AttachmentDetail::Auto,
        };
        let mut request = ResponsesRequest::stateless_replay(
            "model",
            "instructions",
            vec![serde_json::json!({"role":"user","content":[reference.placeholder_part()]})],
            128,
        );
        assert!(matches!(
            store.hydrate_request(&mut request, ApiCapabilities::responses_default()),
            Err(AttachmentError::ProviderUnsupported { .. })
        ));
        Ok(())
    }
}
