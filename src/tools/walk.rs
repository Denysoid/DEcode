use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use tokio_util::sync::CancellationToken;

use super::{
    SandboxRoot, ToolError, check_cancellation, path_has_excluded_component,
    sandbox::{SandboxEntryKind, SandboxPath},
};

const MAX_IGNORE_FILE_BYTES: usize = 1024 * 1024;
const MAX_WALK_CANDIDATES: usize = 100_000;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WalkDiagnostics {
    pub(crate) walk_errors: usize,
    pub(crate) inaccessible_candidates: usize,
    pub(crate) candidate_limit_reached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WalkControl {
    Continue,
    Stop,
}

struct PendingDirectory {
    path: SandboxPath,
    depth: usize,
    matchers: Arc<Vec<Gitignore>>,
}

pub(crate) fn walk_capability<F>(
    sandbox: &SandboxRoot,
    base: &SandboxPath,
    max_depth: usize,
    operation: &'static str,
    cancel: &CancellationToken,
    mut visitor: F,
) -> Result<WalkDiagnostics, ToolError>
where
    F: FnMut(&SandboxPath, SandboxEntryKind) -> Result<WalkControl, ToolError>,
{
    check_cancellation(operation, base.requested_path(), cancel)?;

    let mut diagnostics = WalkDiagnostics::default();
    let ancestor_matchers = load_ancestor_matchers(
        sandbox,
        base.relative_path(),
        operation,
        cancel,
        &mut diagnostics,
    )?;

    let mut stack = vec![PendingDirectory {
        path: base.clone(),
        depth: 0,
        matchers: Arc::new(ancestor_matchers),
    }];
    let mut candidates_seen = 0usize;

    while let Some(pending) = stack.pop() {
        check_cancellation(operation, pending.path.requested_path(), cancel)?;

        if pending.depth >= max_depth {
            continue;
        }

        let (names, entry_errors, directory_limit_reached) =
            match sandbox.directory_entry_names(&pending.path) {
                Ok(value) => value,
                Err(_) if pending.depth > 0 => {
                    diagnostics.inaccessible_candidates =
                        diagnostics.inaccessible_candidates.saturating_add(1);
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
        diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(entry_errors);
        diagnostics.candidate_limit_reached |= directory_limit_reached;

        let mut matchers = pending.matchers.as_ref().clone();
        if let Some(matcher) = load_local_matcher(sandbox, &pending.path, &mut diagnostics) {
            matchers.push(matcher);
        }
        let matchers = Arc::new(matchers);

        let mut child_directories = Vec::new();

        for name in names {
            check_cancellation(operation, pending.path.requested_path(), cancel)?;
            if candidates_seen >= MAX_WALK_CANDIDATES {
                diagnostics.candidate_limit_reached = true;
                return Ok(diagnostics);
            }
            candidates_seen = candidates_seen.saturating_add(1);

            let relative = pending.path.relative_path().join(&name);
            if path_has_excluded_component(&relative) {
                continue;
            }

            let candidate = match sandbox.resolve_candidate(&relative, false) {
                Ok(value) => value,
                Err(_) => {
                    diagnostics.inaccessible_candidates =
                        diagnostics.inaccessible_candidates.saturating_add(1);
                    continue;
                }
            };
            let kind = match sandbox.entry_kind(&candidate) {
                Ok(value) => value,
                Err(_) => {
                    diagnostics.inaccessible_candidates =
                        diagnostics.inaccessible_candidates.saturating_add(1);
                    continue;
                }
            };
            let is_directory = kind == SandboxEntryKind::Directory;

            if sandbox.check_privacy(&candidate, is_directory).is_err() {
                continue;
            }

            if is_ignored(matchers.as_ref(), candidate.relative_path(), is_directory) {
                continue;
            }

            if visitor(&candidate, kind)? == WalkControl::Stop {
                return Ok(diagnostics);
            }

            if is_directory {
                child_directories.push(PendingDirectory {
                    path: candidate,
                    depth: pending.depth.saturating_add(1),
                    matchers: Arc::clone(&matchers),
                });
            }
        }

        // The capability enumerator sorts names. Reverse the pending children
        // so the LIFO stack retains deterministic ascending traversal.
        child_directories.reverse();
        stack.extend(child_directories);
    }

    check_cancellation(operation, base.requested_path(), cancel)?;
    Ok(diagnostics)
}

fn load_ancestor_matchers(
    sandbox: &SandboxRoot,
    base: &Path,
    operation: &'static str,
    cancel: &CancellationToken,
    diagnostics: &mut WalkDiagnostics,
) -> Result<Vec<Gitignore>, ToolError> {
    let mut matchers = Vec::new();
    let mut current = PathBuf::new();
    let components: Vec<_> = base.components().collect();

    if !components.is_empty() {
        let root = sandbox.resolve_candidate(Path::new(""), true)?;
        if let Some(matcher) = load_local_matcher(sandbox, &root, diagnostics) {
            matchers.push(matcher);
        }
    }

    for component in components.iter().take(components.len().saturating_sub(1)) {
        current.push(component.as_os_str());
        check_cancellation(operation, &current, cancel)?;
        let directory = match sandbox.resolve_candidate(&current, true) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.inaccessible_candidates =
                    diagnostics.inaccessible_candidates.saturating_add(1);
                continue;
            }
        };
        if let Some(matcher) = load_local_matcher(sandbox, &directory, diagnostics) {
            matchers.push(matcher);
        }
    }

    Ok(matchers)
}

fn load_local_matcher(
    sandbox: &SandboxRoot,
    directory: &SandboxPath,
    diagnostics: &mut WalkDiagnostics,
) -> Option<Gitignore> {
    let mut builder = GitignoreBuilder::new(directory.relative_path());
    #[cfg(windows)]
    if builder.case_insensitive(true).is_err() {
        diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
        return None;
    }
    let mut found_rules = false;

    for ignore_name in [
        Path::new(".gitignore"),
        Path::new(".ignore"),
        Path::new(".git/info/exclude"),
    ] {
        let relative = directory.relative_path().join(ignore_name);
        let path = match sandbox.resolve_candidate(&relative, false) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
                continue;
            }
        };
        let bytes = match sandbox.read_regular_file_limited(&path, MAX_IGNORE_FILE_BYTES) {
            Ok(value) => value,
            Err(super::SandboxError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                continue;
            }
            Err(_) => {
                diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
                continue;
            }
        };
        let contents = match std::str::from_utf8(&bytes) {
            Ok(value) => value,
            Err(_) => {
                diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
                continue;
            }
        };

        for (index, line) in contents.lines().enumerate() {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            if builder.add_line(Some(relative.clone()), line).is_err() {
                diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
            } else {
                found_rules = true;
            }
        }
    }

    if !found_rules {
        return None;
    }

    match builder.build() {
        Ok(matcher) => Some(matcher),
        Err(_) => {
            diagnostics.walk_errors = diagnostics.walk_errors.saturating_add(1);
            None
        }
    }
}

fn is_ignored(matchers: &[Gitignore], path: &Path, is_directory: bool) -> bool {
    for matcher in matchers.iter().rev() {
        match matcher.matched(path, is_directory) {
            Match::Ignore(_) => return true,
            Match::Whitelist(_) => return false,
            Match::None => {}
        }
    }

    false
}
