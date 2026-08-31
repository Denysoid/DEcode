use std::path::Path;

use tokio_util::sync::CancellationToken;

use super::{
    MAX_MODEL_PATH_BYTES, ReviewedWriteBaseline, SandboxRoot, ToolError, check_cancellation,
    ensure_input_limit, reject_excluded_tree,
    sandbox::{AtomicWriteOptions, SandboxEntryKind},
    sanitize_tool_path,
    walk::{WalkControl, walk_capability},
};

pub const MAX_READ_FILE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_WRITE_FILE_BYTES: usize = 16 * 1024 * 1024;

const TRUNCATION_MARKER: &str = "\n...[directory listing truncated]...\n";
const MAX_LIST_DEPTH: usize = 128;
const MAX_LIST_ENTRIES: usize = 100_000;
const MAX_LIST_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListDirectoryOptions {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_output_bytes: usize,
}

impl Default for ListDirectoryOptions {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_entries: 1_000,
            max_output_bytes: 128 * 1024,
        }
    }
}

pub async fn read_file(sandbox: &SandboxRoot, requested: &str) -> Result<String, ToolError> {
    read_file_cancellable(sandbox, requested, CancellationToken::new()).await
}

pub(crate) async fn read_file_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
    let sandbox = sandbox.clone();
    let requested = requested.to_owned();

    tokio::task::spawn_blocking(move || {
        check_cancellation("read_file", Path::new(&requested), &cancel)?;
        sandbox.verify_ambient_root_identity()?;
        let path = sandbox.model_file_path(&requested)?;
        let bytes = sandbox.read_regular_file_limited(&path, MAX_READ_FILE_BYTES)?;
        check_cancellation("read_file", path.requested_path(), &cancel)?;

        String::from_utf8(bytes).map_err(|source| ToolError::InvalidUtf8 {
            path: path.requested_path().to_path_buf(),
            source,
        })
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "read_file",
        source,
    })?
}

pub async fn write_file(
    sandbox: &SandboxRoot,
    requested: &str,
    content: &str,
) -> Result<String, ToolError> {
    write_file_cancellable(sandbox, requested, content, CancellationToken::new()).await
}

pub(crate) async fn write_file_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    content: &str,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
    ensure_input_limit("content", content.len(), MAX_WRITE_FILE_BYTES)?;
    let sandbox = sandbox.clone();
    let requested = requested.to_owned();
    let content = content.to_owned();

    tokio::task::spawn_blocking(move || {
        check_cancellation("write_file", Path::new(&requested), &cancel)?;
        sandbox.verify_ambient_root_identity()?;
        let path = sandbox.model_file_path(&requested)?;
        let byte_count = content.len();

        sandbox.atomic_write_cancellable(
            &path,
            content.as_bytes(),
            AtomicWriteOptions::capture_destination(true, MAX_WRITE_FILE_BYTES),
            &cancel,
            "write_file",
        )?;

        Ok(format!(
            "wrote {byte_count} bytes to {}",
            sanitize_tool_path(path.requested_path())
        ))
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "write_file",
        source,
    })?
}

pub(crate) async fn capture_write_file_baseline(
    sandbox: &SandboxRoot,
    requested: &str,
    cancel: CancellationToken,
) -> Result<ReviewedWriteBaseline, ToolError> {
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
    let sandbox = sandbox.clone();
    let requested = requested.to_owned();
    tokio::task::spawn_blocking(move || {
        check_cancellation("preview write_file", Path::new(&requested), &cancel)?;
        sandbox.verify_ambient_root_identity()?;
        let path = sandbox.model_file_path(&requested)?;
        match sandbox.read_regular_file_limited(&path, MAX_READ_FILE_BYTES) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(ReviewedWriteBaseline::Existing)
                .map_err(|source| ToolError::InvalidUtf8 {
                    path: path.requested_path().to_path_buf(),
                    source,
                }),
            Err(super::SandboxError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(ReviewedWriteBaseline::Missing)
            }
            Err(error) => Err(ToolError::Sandbox(error)),
        }
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "preview write_file",
        source,
    })?
}

pub(crate) async fn write_file_reviewed_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    content: &str,
    baseline: ReviewedWriteBaseline,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
    ensure_input_limit("content", content.len(), MAX_WRITE_FILE_BYTES)?;
    let sandbox = sandbox.clone();
    let requested = requested.to_owned();
    let content = content.to_owned();
    tokio::task::spawn_blocking(move || {
        check_cancellation("reviewed write_file", Path::new(&requested), &cancel)?;
        sandbox.verify_ambient_root_identity()?;
        let path = sandbox.model_file_path(&requested)?;
        let options = match &baseline {
            ReviewedWriteBaseline::Missing => {
                AtomicWriteOptions::expect_missing(true, MAX_WRITE_FILE_BYTES)
            }
            ReviewedWriteBaseline::Existing(expected) => {
                AtomicWriteOptions::expect_content(true, expected.as_bytes(), MAX_WRITE_FILE_BYTES)
            }
        };
        let byte_count = content.len();
        sandbox.atomic_write_cancellable(
            &path,
            content.as_bytes(),
            options,
            &cancel,
            "reviewed write_file",
        )?;
        Ok(format!(
            "wrote {byte_count} reviewed bytes to {}",
            sanitize_tool_path(path.requested_path())
        ))
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "reviewed write_file",
        source,
    })?
}

pub async fn list_directory(sandbox: &SandboxRoot, requested: &str) -> Result<String, ToolError> {
    list_directory_cancellable(sandbox, requested, CancellationToken::new()).await
}

pub(crate) async fn list_directory_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    list_directory_with_options_cancellable(
        sandbox,
        requested,
        ListDirectoryOptions::default(),
        cancel,
    )
    .await
}

pub async fn list_directory_with_options(
    sandbox: &SandboxRoot,
    requested: &str,
    options: ListDirectoryOptions,
) -> Result<String, ToolError> {
    list_directory_with_options_cancellable(sandbox, requested, options, CancellationToken::new())
        .await
}

pub(crate) async fn list_directory_with_options_cancellable(
    sandbox: &SandboxRoot,
    requested: &str,
    options: ListDirectoryOptions,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    validate_list_options(options)?;
    ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;

    let sandbox = sandbox.clone();
    let requested = requested.to_owned();

    tokio::task::spawn_blocking(move || list_directory_sync(&sandbox, &requested, options, &cancel))
        .await
        .map_err(|source| ToolError::WorkerTask {
            operation: "list_directory",
            source,
        })?
}

fn validate_list_options(options: ListDirectoryOptions) -> Result<(), ToolError> {
    if options.max_entries == 0 {
        return Err(ToolError::InvalidLimit {
            name: "list_directory.max_entries",
        });
    }

    if options.max_output_bytes == 0 {
        return Err(ToolError::InvalidLimit {
            name: "list_directory.max_output_bytes",
        });
    }

    if options.max_output_bytes < TRUNCATION_MARKER.len() {
        return Err(ToolError::LimitTooSmall {
            name: "list_directory.max_output_bytes",
            minimum: TRUNCATION_MARKER.len(),
            actual: options.max_output_bytes,
        });
    }

    for (name, actual, maximum) in [
        (
            "list_directory.max_depth",
            options.max_depth,
            MAX_LIST_DEPTH,
        ),
        (
            "list_directory.max_entries",
            options.max_entries,
            MAX_LIST_ENTRIES,
        ),
        (
            "list_directory.max_output_bytes",
            options.max_output_bytes,
            MAX_LIST_OUTPUT_BYTES,
        ),
    ] {
        if actual > maximum {
            return Err(ToolError::LimitTooLarge {
                name,
                maximum,
                actual,
            });
        }
    }

    Ok(())
}

fn list_directory_sync(
    sandbox: &SandboxRoot,
    requested: &str,
    options: ListDirectoryOptions,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    check_cancellation("list_directory", Path::new(requested), cancel)?;
    sandbox.verify_ambient_root_identity()?;
    let directory = sandbox.model_directory_path(requested)?;

    reject_excluded_tree("list_directory", directory.relative_path())?;

    sandbox.ensure_directory(&directory)?;

    let mut output = String::new();
    let mut listed_entries = 0usize;
    let mut special_entries = 0usize;
    let mut truncated = false;

    let diagnostics = walk_capability(
        sandbox,
        &directory,
        options.max_depth,
        "list_directory",
        cancel,
        |candidate, kind| {
            if kind == SandboxEntryKind::Other {
                special_entries = special_entries.saturating_add(1);
                return Ok(WalkControl::Continue);
            }

            if listed_entries >= options.max_entries {
                truncated = true;
                return Ok(WalkControl::Stop);
            }

            let suffix = if kind == SandboxEntryKind::Directory {
                "/"
            } else {
                ""
            };
            let line = format!(
                "{}{suffix}\n",
                sanitize_tool_path(candidate.relative_path())
            );

            if !push_complete(&mut output, &line, options.max_output_bytes) {
                truncated = true;
                return Ok(WalkControl::Stop);
            }

            listed_entries = listed_entries.saturating_add(1);
            Ok(WalkControl::Continue)
        },
    )?;

    truncated |= diagnostics.candidate_limit_reached;

    check_cancellation("list_directory", directory.requested_path(), cancel)?;

    if output.is_empty() {
        output.push_str("(directory is empty or has no visible entries)\n");
    }

    if diagnostics.walk_errors > 0
        || diagnostics.inaccessible_candidates > 0
        || diagnostics.candidate_limit_reached
        || special_entries > 0
    {
        output.push_str(&format!(
            "\n[listing diagnostics: walker errors: {}; \
             inaccessible or changed candidates: \
             {}; candidate limit reached: {}; special files skipped: {special_entries}]\n",
            diagnostics.walk_errors,
            diagnostics.inaccessible_candidates,
            diagnostics.candidate_limit_reached,
        ));
    }

    Ok(cap_output(
        output,
        options.max_output_bytes,
        truncated,
        TRUNCATION_MARKER,
    ))
}

fn push_complete(output: &mut String, value: &str, limit: usize) -> bool {
    let Some(new_length) = output.len().checked_add(value.len()) else {
        return false;
    };

    if new_length > limit {
        return false;
    }

    output.push_str(value);
    true
}

fn cap_output(mut output: String, limit: usize, already_truncated: bool, marker: &str) -> String {
    let needs_marker = already_truncated || output.len() > limit;

    if !needs_marker {
        return output;
    }

    let content_limit = limit.saturating_sub(marker.len());
    truncate_to_boundary(&mut output, content_limit);
    output.push_str(marker);
    output
}

fn truncate_to_boundary(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }

    let mut boundary = limit;

    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }

    value.truncate(boundary);
}
