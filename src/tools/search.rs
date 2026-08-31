use std::path::Path;

use regex::Regex;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    MAX_MODEL_PATH_BYTES, MAX_SEARCH_PATTERN_BYTES, SandboxError, SandboxRoot, ToolError,
    check_cancellation, ensure_input_limit, reject_excluded_tree,
    sandbox::SandboxEntryKind,
    sanitize_tool_path, sanitize_tool_text,
    walk::{WalkControl, walk_capability},
};

const BINARY_PROBE_BYTES: usize = 8 * 1024;
const RESULT_LINE_MAX_CHARS: usize = 360;
const RESULT_CONTEXT_BEFORE_CHARS: usize = 100;
const TRUNCATION_MARKER: &str = "\n...[search results truncated]...\n";
const MAX_SEARCH_MATCHES: usize = 100_000;
const MAX_SEARCH_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_SEARCH_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SEARCH_DEPTH: usize = 128;

#[derive(Debug, Error)]
pub enum SearchError {
    #[error("search pattern must not be empty")]
    EmptyPattern,

    #[error("invalid search regular expression: {source}")]
    InvalidRegex {
        #[source]
        source: regex::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCodeOptions {
    pub max_matches: usize,
    pub max_output_bytes: usize,
    pub max_file_bytes: usize,
    pub max_depth: usize,
}

impl Default for SearchCodeOptions {
    fn default() -> Self {
        Self {
            max_matches: 100,
            max_output_bytes: 64 * 1024,
            max_file_bytes: 4 * 1024 * 1024,
            max_depth: 64,
        }
    }
}

pub async fn search_code(
    sandbox: &SandboxRoot,
    pattern: &str,
    requested_path: Option<&str>,
) -> Result<String, ToolError> {
    search_code_cancellable(sandbox, pattern, requested_path, CancellationToken::new()).await
}

pub(crate) async fn search_code_cancellable(
    sandbox: &SandboxRoot,
    pattern: &str,
    requested_path: Option<&str>,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    search_code_with_options_cancellable(
        sandbox,
        pattern,
        requested_path,
        SearchCodeOptions::default(),
        cancel,
    )
    .await
}

pub async fn search_code_with_options(
    sandbox: &SandboxRoot,
    pattern: &str,
    requested_path: Option<&str>,
    options: SearchCodeOptions,
) -> Result<String, ToolError> {
    search_code_with_options_cancellable(
        sandbox,
        pattern,
        requested_path,
        options,
        CancellationToken::new(),
    )
    .await
}

pub(crate) async fn search_code_with_options_cancellable(
    sandbox: &SandboxRoot,
    pattern: &str,
    requested_path: Option<&str>,
    options: SearchCodeOptions,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    validate_search_options(options)?;
    ensure_input_limit("pattern", pattern.len(), MAX_SEARCH_PATTERN_BYTES)?;
    ensure_input_limit(
        "path",
        requested_path.unwrap_or(".").len(),
        MAX_MODEL_PATH_BYTES,
    )?;

    let sandbox = sandbox.clone();
    let pattern = pattern.to_owned();
    let requested_path = requested_path.unwrap_or(".").to_owned();

    tokio::task::spawn_blocking(move || {
        search_code_sync(&sandbox, &pattern, &requested_path, options, &cancel)
    })
    .await
    .map_err(|source| ToolError::WorkerTask {
        operation: "search_code",
        source,
    })?
}

fn validate_search_options(options: SearchCodeOptions) -> Result<(), ToolError> {
    if options.max_matches == 0 {
        return Err(ToolError::InvalidLimit {
            name: "search_code.max_matches",
        });
    }

    if options.max_output_bytes == 0 {
        return Err(ToolError::InvalidLimit {
            name: "search_code.max_output_bytes",
        });
    }

    if options.max_file_bytes == 0 {
        return Err(ToolError::InvalidLimit {
            name: "search_code.max_file_bytes",
        });
    }

    if options.max_output_bytes < TRUNCATION_MARKER.len() {
        return Err(ToolError::LimitTooSmall {
            name: "search_code.max_output_bytes",
            minimum: TRUNCATION_MARKER.len(),
            actual: options.max_output_bytes,
        });
    }

    for (name, actual, maximum) in [
        (
            "search_code.max_matches",
            options.max_matches,
            MAX_SEARCH_MATCHES,
        ),
        (
            "search_code.max_output_bytes",
            options.max_output_bytes,
            MAX_SEARCH_OUTPUT_BYTES,
        ),
        (
            "search_code.max_file_bytes",
            options.max_file_bytes,
            MAX_SEARCH_FILE_BYTES,
        ),
        ("search_code.max_depth", options.max_depth, MAX_SEARCH_DEPTH),
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

fn search_code_sync(
    sandbox: &SandboxRoot,
    pattern: &str,
    requested_path: &str,
    options: SearchCodeOptions,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    check_cancellation("search_code", Path::new(requested_path), cancel)?;
    sandbox.verify_ambient_root_identity()?;
    if pattern.is_empty() {
        return Err(SearchError::EmptyPattern.into());
    }

    let expression = Regex::new(pattern).map_err(|source| SearchError::InvalidRegex { source })?;

    let base = sandbox.model_directory_path(requested_path)?;

    reject_excluded_tree("search_code", base.relative_path())?;
    sandbox.ensure_directory(&base)?;

    // Traversal, ignore-file loading, metadata checks, and file reads all
    // stay rooted in SandboxRoot capabilities.
    let mut body = String::new();
    let mut matches_seen = 0usize;
    let mut matches_shown = 0usize;
    let mut binary_or_non_utf8_files = 0usize;
    let mut oversized_files = 0usize;
    let mut special_files = 0usize;
    let mut inaccessible_candidates = 0usize;
    let mut truncated = false;

    let diagnostics = walk_capability(
        sandbox,
        &base,
        options.max_depth,
        "search_code",
        cancel,
        |candidate, kind| {
            match kind {
                SandboxEntryKind::Directory => return Ok(WalkControl::Continue),
                SandboxEntryKind::Other => {
                    special_files = special_files.saturating_add(1);
                    return Ok(WalkControl::Continue);
                }
                SandboxEntryKind::File => {}
            }

            let bytes = match sandbox.read_regular_file_limited(candidate, options.max_file_bytes) {
                Ok(value) => value,
                Err(SandboxError::FileTooLarge { .. }) => {
                    oversized_files = oversized_files.saturating_add(1);
                    return Ok(WalkControl::Continue);
                }
                Err(SandboxError::NotRegularFile { .. }) => {
                    special_files = special_files.saturating_add(1);
                    return Ok(WalkControl::Continue);
                }
                Err(_) => {
                    inaccessible_candidates = inaccessible_candidates.saturating_add(1);
                    return Ok(WalkControl::Continue);
                }
            };

            if is_binary(&bytes) {
                binary_or_non_utf8_files = binary_or_non_utf8_files.saturating_add(1);
                return Ok(WalkControl::Continue);
            }

            let content = match std::str::from_utf8(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    binary_or_non_utf8_files = binary_or_non_utf8_files.saturating_add(1);
                    return Ok(WalkControl::Continue);
                }
            };

            for (line_index, line) in content.lines().enumerate() {
                check_cancellation("search_code", candidate.requested_path(), cancel)?;
                for found in expression.find_iter(line) {
                    if matches_seen >= options.max_matches {
                        truncated = true;
                        return Ok(WalkControl::Stop);
                    }

                    matches_seen = matches_seen.saturating_add(1);

                    let column = line
                        .get(..found.start())
                        .map_or(0, |prefix| prefix.chars().count())
                        .saturating_add(1);

                    let excerpt = excerpt_around_match(
                        line,
                        found.start(),
                        RESULT_LINE_MAX_CHARS,
                        RESULT_CONTEXT_BEFORE_CHARS,
                    );

                    let record = format!(
                        "{}:{}:{}: {}\n",
                        sanitize_tool_path(candidate.relative_path()),
                        line_index.saturating_add(1),
                        column,
                        sanitize_tool_text(&excerpt)
                    );

                    if !push_complete(&mut body, &record, options.max_output_bytes) {
                        truncated = true;
                        return Ok(WalkControl::Stop);
                    }

                    matches_shown = matches_shown.saturating_add(1);
                }
            }

            Ok(WalkControl::Continue)
        },
    )?;

    inaccessible_candidates =
        inaccessible_candidates.saturating_add(diagnostics.inaccessible_candidates);
    let walk_errors = diagnostics.walk_errors;
    truncated |= diagnostics.candidate_limit_reached;

    check_cancellation("search_code", base.requested_path(), cancel)?;

    let header = if matches_seen == 0 {
        "no matches found\n".to_owned()
    } else {
        format!(
            "{matches_seen} match(es) observed; \
             {matches_shown} shown\n"
        )
    };

    let mut output = String::new();
    output.push_str(&header);
    output.push_str(&body);

    if binary_or_non_utf8_files > 0
        || oversized_files > 0
        || special_files > 0
        || inaccessible_candidates > 0
        || walk_errors > 0
        || diagnostics.candidate_limit_reached
    {
        output.push_str(&format!(
            "\n[search diagnostics: binary/non-UTF-8 files skipped: \
             {binary_or_non_utf8_files}; oversized files skipped: \
             {oversized_files}; special files skipped: \
             {special_files}; inaccessible or changed candidates: \
             {inaccessible_candidates}; walker errors: \
             {walk_errors}; candidate limit reached: {}]\n",
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

fn is_binary(bytes: &[u8]) -> bool {
    let probe_length = bytes.len().min(BINARY_PROBE_BYTES);

    match bytes.get(..probe_length) {
        Some(probe) => probe.contains(&0),
        None => true,
    }
}

fn excerpt_around_match(
    line: &str,
    match_byte_start: usize,
    max_chars: usize,
    context_before_chars: usize,
) -> String {
    let match_char_start = line
        .get(..match_byte_start)
        .map_or(0, |prefix| prefix.chars().count());

    let excerpt_start = match_char_start.saturating_sub(context_before_chars);

    let mut excerpt: String = line.chars().skip(excerpt_start).take(max_chars).collect();

    if excerpt_start > 0 {
        excerpt.insert(0, '\u{2026}');
    }

    let consumed_end = excerpt_start.saturating_add(max_chars);

    if line.chars().count() > consumed_end {
        excerpt.push('\u{2026}');
    }

    excerpt
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
