use std::{
    fmt,
    path::{Path, PathBuf},
};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    MAX_MODEL_PATH_BYTES, SandboxRoot, ToolError, check_cancellation, ensure_input_limit,
    sandbox::{AtomicWriteOptions, SandboxPath},
    sanitize_tool_path,
};

pub const MAX_PATCH_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PATCH_RESULT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchHint {
    None,
    WhitespaceDifference,
}

impl fmt::Display for PatchHint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => Ok(()),
            Self::WhitespaceDifference => formatter.write_str(
                "; the file appears to differ only in whitespace, \
                 indentation, or line endings; request the file again \
                 and provide an exact search block",
            ),
        }
    }
}

#[derive(Debug, Error)]
pub enum PatchError {
    #[error(
        "search block was not found in {path:?}: \
         no exact occurrence{hint}"
    )]
    NotFound { path: PathBuf, hint: PatchHint },

    #[error(
        "search block occurs {count} times in {path:?}; the patch is \
         ambiguous and needs a more unique search block"
    )]
    Ambiguous { path: PathBuf, count: usize },

    #[error("file {path:?} is not valid UTF-8: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error("the search block for {path:?} must not be empty")]
    EmptySearch { path: PathBuf },

    #[error("patched content size overflowed for {path:?}")]
    ContentSizeOverflow { path: PathBuf },

    #[error("internal UTF-8 boundary inconsistency while patching {path:?}")]
    InvalidBoundary { path: PathBuf },
}

pub async fn apply_patch(
    sandbox: &SandboxRoot,
    requested: &str,
    search: &str,
    replace: &str,
) -> Result<String, ToolError> {
    apply_patch_cancellable(
        sandbox,
        requested,
        search,
        replace,
        CancellationToken::new(),
    )
    .await
}

pub(crate) async fn apply_patch_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    search: &str,
    replace: &str,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
    ensure_input_limit("search", search.len(), MAX_PATCH_FILE_BYTES)?;
    ensure_input_limit("replace", replace.len(), MAX_PATCH_RESULT_BYTES)?;
    let sandbox = sandbox.clone();
    let requested = requested.to_owned();
    let search = search.to_owned();
    let replace = replace.to_owned();

    tokio::task::spawn_blocking(move || {
        apply_patch_sync(&sandbox, &requested, &search, &replace, &cancel)
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "apply_patch",
        source,
    })?
}

fn apply_patch_sync(
    sandbox: &SandboxRoot,
    requested: &str,
    search: &str,
    replace: &str,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    check_cancellation("apply_patch", Path::new(requested), cancel)?;
    sandbox.verify_ambient_root_identity()?;
    let path = sandbox.model_file_path(requested)?;

    sandbox.with_target_write_lock_cancellable(&path, cancel, "apply_patch", || {
        apply_patch_under_lock(sandbox, &path, search, replace, cancel)
    })
}

fn apply_patch_under_lock(
    sandbox: &SandboxRoot,
    path: &SandboxPath,
    search: &str,
    replace: &str,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    let error_path = path.requested_path().to_path_buf();

    if search.is_empty() {
        return Err(PatchError::EmptySearch { path: error_path }.into());
    }

    // Файл всегда перечитывается непосредственно перед патчем.
    let bytes = sandbox.read_regular_file_limited(path, MAX_PATCH_FILE_BYTES)?;
    check_cancellation("apply_patch", path.requested_path(), cancel)?;

    let content = String::from_utf8(bytes).map_err(|source| PatchError::InvalidUtf8 {
        path: path.requested_path().to_path_buf(),
        source,
    })?;

    let (first_start, occurrence_count) = overlapping_match_count(&content, search);
    let first_start = match first_start {
        Some(position) => position,
        None => {
            let hint = if has_whitespace_normalized_match(&content, search) {
                PatchHint::WhitespaceDifference
            } else {
                PatchHint::None
            };

            return Err(PatchError::NotFound {
                path: path.requested_path().to_path_buf(),
                hint,
            }
            .into());
        }
    };

    if occurrence_count > 1 {
        return Err(PatchError::Ambiguous {
            path: path.requested_path().to_path_buf(),
            count: occurrence_count,
        }
        .into());
    }

    let search_end =
        first_start
            .checked_add(search.len())
            .ok_or_else(|| PatchError::ContentSizeOverflow {
                path: path.requested_path().to_path_buf(),
            })?;

    let prefix = content
        .get(..first_start)
        .ok_or_else(|| PatchError::InvalidBoundary {
            path: path.requested_path().to_path_buf(),
        })?;

    let suffix = content
        .get(search_end..)
        .ok_or_else(|| PatchError::InvalidBoundary {
            path: path.requested_path().to_path_buf(),
        })?;

    let capacity = prefix
        .len()
        .checked_add(replace.len())
        .and_then(|value| value.checked_add(suffix.len()))
        .ok_or_else(|| PatchError::ContentSizeOverflow {
            path: path.requested_path().to_path_buf(),
        })?;

    if capacity > MAX_PATCH_RESULT_BYTES {
        return Err(ToolError::InputTooLarge {
            field: "patched_content",
            actual_bytes: capacity,
            limit_bytes: MAX_PATCH_RESULT_BYTES,
        });
    }

    let mut patched = String::with_capacity(capacity);
    patched.push_str(prefix);
    patched.push_str(replace);
    patched.push_str(suffix);

    // Никакой нормализации line endings не выполняется. Кроме явно
    // заменяемого диапазона байты сохраняются без изменений.
    sandbox.atomic_write_under_lock(
        path,
        patched.as_bytes(),
        AtomicWriteOptions::expect_content(false, content.as_bytes(), MAX_PATCH_FILE_BYTES),
        Some((cancel, "apply_patch")),
    )?;

    Ok(format!(
        "patched {}: exactly one occurrence replaced",
        sanitize_tool_path(path.requested_path())
    ))
}

fn has_whitespace_normalized_match(content: &str, search: &str) -> bool {
    let normalized_search = normalize_whitespace(search);

    if normalized_search.is_empty() {
        return false;
    }

    normalize_whitespace(content).contains(&normalized_search)
}

fn overlapping_match_count(content: &str, search: &str) -> (Option<usize>, usize) {
    let haystack = content.as_bytes();
    let needle = search.as_bytes();
    let mut prefix = Vec::with_capacity(needle.len());
    prefix.push(0_u32);
    let mut matched = 0usize;
    for &byte in &needle[1..] {
        while matched > 0 && byte != needle[matched] {
            matched = prefix[matched - 1] as usize;
        }
        if byte == needle[matched] {
            matched = matched.saturating_add(1);
        }
        prefix.push(matched as u32);
    }

    let mut first = None;
    let mut count = 0usize;
    matched = 0;
    for (index, &byte) in haystack.iter().enumerate() {
        while matched > 0 && byte != needle[matched] {
            matched = prefix[matched - 1] as usize;
        }
        if byte == needle[matched] {
            matched = matched.saturating_add(1);
        }
        if matched == needle.len() {
            first.get_or_insert(index + 1 - needle.len());
            count = count.saturating_add(1);
            matched = prefix[matched - 1] as usize;
        }
    }
    (first, count)
}

fn normalize_whitespace(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len());
    let mut pending_separator = false;

    for character in value.chars() {
        if character.is_whitespace() {
            if !normalized.is_empty() {
                pending_separator = true;
            }

            continue;
        }

        if pending_separator {
            normalized.push(' ');
            pending_separator = false;
        }

        normalized.push(character);
    }

    normalized
}
