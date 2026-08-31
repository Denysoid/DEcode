pub mod exec;
pub mod fs_ops;
pub mod patch;
pub mod patch_review;
pub mod sandbox;
pub mod search;
mod walk;

use std::{
    collections::HashSet,
    future::Future,
    panic::AssertUnwindSafe,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::FutureExt as _;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::parser::{ToolAction, ToolOutcome};
use crate::privacy::PrivacyShield;

pub use fs_ops::{
    ListDirectoryOptions, MAX_READ_FILE_BYTES, MAX_WRITE_FILE_BYTES, list_directory,
    list_directory_with_options, read_file, write_file,
};
pub use patch::{MAX_PATCH_FILE_BYTES, MAX_PATCH_RESULT_BYTES, PatchError, PatchHint, apply_patch};
pub use patch_review::{PatchHunk, PatchReview, PatchReviewError, PatchSelection};
pub use sandbox::{PathViolation, SandboxError, SandboxRoot};
pub use search::{SearchCodeOptions, SearchError, search_code, search_code_with_options};

pub use exec::{
    ApprovalBinding, ApprovalNonce, CommandApproval, CommandDigest, ConfirmationDecision,
    ConfirmationReason, DEFAULT_EXEC_TIMEOUT, DEFAULT_MAX_OUTPUT_BYTES, ExecError, ExecOptions,
    MAX_EXEC_TIMEOUT, MAX_OUTPUT_BYTES, ShellConfirmationMode, StrictAllowlistEntry,
    deferred_reaper_count, drain_deferred_reapers,
};

pub const MAX_MODEL_PATH_BYTES: usize = 16 * 1024;
pub const MAX_SEARCH_PATTERN_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_BYTES: usize = 128 * 1024;
const MAX_CONSUMED_APPROVAL_NONCES: usize = 65_536;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewedWriteBaseline {
    Missing,
    Existing(String),
}

#[derive(Debug, Default)]
struct ApprovalReplayRegistry {
    epoch: Option<u64>,
    nonces: HashSet<ApprovalNonce>,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error(transparent)]
    Sandbox(#[from] SandboxError),

    #[error(transparent)]
    Patch(#[from] PatchError),

    #[error(transparent)]
    Search(#[from] SearchError),

    #[error(transparent)]
    Exec(#[from] exec::ExecError),

    #[error("file {path:?} is not valid UTF-8: {source}")]
    InvalidUtf8 {
        path: PathBuf,
        #[source]
        source: std::string::FromUtf8Error,
    },

    #[error(
        "blocking worker for operation `{operation}` could not complete: \
         {source}"
    )]
    WorkerTask {
        operation: &'static str,
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("configuration limit `{name}` must be greater than zero")]
    InvalidLimit { name: &'static str },

    #[error(
        "configuration limit `{name}` must be at least {minimum}, \
         but was {actual}"
    )]
    LimitTooSmall {
        name: &'static str,
        minimum: usize,
        actual: usize,
    },

    #[error("configuration limit `{name}` must not exceed {maximum}, but was {actual}")]
    LimitTooLarge {
        name: &'static str,
        maximum: usize,
        actual: usize,
    },

    #[error(
        "tool input `{field}` is {actual_bytes} bytes, exceeding the \
         permitted {limit_bytes} bytes"
    )]
    InputTooLarge {
        field: &'static str,
        actual_bytes: usize,
        limit_bytes: usize,
    },

    #[error(
        "operation `{operation}` is not allowed to traverse excluded \
         directory {path:?}"
    )]
    ExcludedPath {
        operation: &'static str,
        path: PathBuf,
    },
}

/// Single capability-rooted entry point for every model tool action.
///
/// The workspace directory is opened once when the runner is constructed.
/// Every later filesystem operation is resolved through that same
/// [`SandboxRoot`]. Shell commands are deliberately different from file
/// tools. The default shell policy requires an exact, consumed
/// [`CommandApproval`] for every command. An explicitly configured strict
/// allowlist may run a tiny set of read-only programs directly, without a
/// shell.
#[derive(Debug, Clone)]
pub struct ToolRunner {
    sandbox: SandboxRoot,
    exec_options: ExecOptions,
    approval_replay_registry: Arc<Mutex<ApprovalReplayRegistry>>,
}

impl ToolRunner {
    pub fn new(workspace_root: &Path, exec_timeout: Duration) -> Result<Self, ToolError> {
        Self::with_exec_options(
            workspace_root,
            ExecOptions::new(exec_timeout, DEFAULT_MAX_OUTPUT_BYTES),
        )
    }

    pub fn with_exec_options(
        workspace_root: &Path,
        exec_options: ExecOptions,
    ) -> Result<Self, ToolError> {
        let privacy =
            PrivacyShield::load_project_only(workspace_root).map_err(SandboxError::from)?;
        Self::with_exec_options_and_privacy(workspace_root, exec_options, privacy)
    }

    pub(crate) fn with_exec_options_and_privacy(
        workspace_root: &Path,
        exec_options: ExecOptions,
        privacy: PrivacyShield,
    ) -> Result<Self, ToolError> {
        exec_options.validate()?;

        Ok(Self {
            sandbox: SandboxRoot::open_with_privacy(workspace_root, privacy)?,
            exec_options,
            approval_replay_registry: Arc::new(Mutex::new(ApprovalReplayRegistry::default())),
        })
    }

    #[must_use]
    pub(crate) fn sandbox_root(&self) -> SandboxRoot {
        self.sandbox.clone()
    }

    pub(crate) fn validate_model_file_path(&self, requested: &str) -> Result<(), ToolError> {
        ensure_input_limit("path", requested.len(), MAX_MODEL_PATH_BYTES)?;
        self.sandbox.model_file_path(requested)?;
        Ok(())
    }

    /// Conservative compatibility helper: returns `true` for every command.
    /// New orchestration code should use [`Self::action_requires_confirmation`]
    /// so an explicitly configured strict allowlist can take effect.
    #[must_use]
    pub const fn requires_confirmation(action: &ToolAction) -> bool {
        matches!(action, ToolAction::ExecuteCommand { .. })
    }

    /// Applies this runner's configured shell policy. File actions never
    /// require confirmation; a strict-allowlisted direct command may run
    /// without it only when the runner explicitly opted into that mode.
    #[must_use]
    pub fn action_requires_confirmation(&self, action: &ToolAction) -> bool {
        self.action_confirmation_decision(action)
            .requires_confirmation()
    }

    /// Returns the complete shell-policy decision for an action. File tools
    /// are always represented as auto-approved; this does not bypass their
    /// sandbox, privacy, patch-review, or atomic-write checks.
    #[must_use]
    pub fn action_confirmation_decision(&self, action: &ToolAction) -> ConfirmationDecision {
        match action {
            ToolAction::ExecuteCommand {
                command,
                requires_confirmation,
            } => exec::confirmation_decision_with_options(
                command,
                *requires_confirmation,
                &self.exec_options,
            ),
            _ => ConfirmationDecision::AutoApproved,
        }
    }

    /// Executes one action and converts all ordinary tool errors into the
    /// canonical [`ToolOutcome`].
    ///
    /// `approval` is ignored for file tools. Under the default command policy
    /// it must contain an exact approval for both command text and the
    /// original model flag. Use [`Self::execute_action_bound`] for replay-safe
    /// one-shot approval semantics.
    pub async fn execute_action(
        &self,
        action: &ToolAction,
        approval: Option<CommandApproval>,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        self.execute_action_inner(action, approval, None, None, None, cancel)
            .await
    }

    /// Executes an action with an approval cryptographically associated by
    /// the caller with an epoch, turn, action, nonce, and command digest.
    /// Bound approval nonces are consumed once per runner and replay attempts
    /// fail closed.
    pub async fn execute_action_bound(
        &self,
        action: &ToolAction,
        approval: Option<CommandApproval>,
        binding: ApprovalBinding,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        self.execute_action_inner(action, approval, Some(binding), None, None, cancel)
            .await
    }

    /// Bound execution with a trusted, pre-selected timeout rule. The
    /// approval policy and one-shot binding remain identical to
    /// `execute_action_bound`.
    pub async fn execute_action_bound_with_timeout(
        &self,
        action: &ToolAction,
        approval: Option<CommandApproval>,
        binding: ApprovalBinding,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        self.execute_action_inner(action, approval, Some(binding), Some(timeout), None, cancel)
            .await
    }

    pub(crate) async fn capture_write_file_baseline(
        &self,
        path: &str,
        cancel: CancellationToken,
    ) -> Result<ReviewedWriteBaseline, ToolError> {
        fs_ops::capture_write_file_baseline(&self.sandbox, path, cancel).await
    }

    pub(crate) async fn execute_reviewed_write_bound_with_timeout(
        &self,
        action: &ToolAction,
        binding: ApprovalBinding,
        timeout: Duration,
        baseline: ReviewedWriteBaseline,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        self.execute_action_inner(
            action,
            None,
            Some(binding),
            Some(timeout),
            Some(baseline),
            cancel,
        )
        .await
    }

    #[tracing::instrument(
        name = "tool.execute",
        level = "info",
        skip_all,
        fields(tool = action.tool_name(), status = "active")
    )]
    async fn execute_action_inner(
        &self,
        action: &ToolAction,
        approval: Option<CommandApproval>,
        expected_binding: Option<ApprovalBinding>,
        timeout_override: Option<Duration>,
        reviewed_write_baseline: Option<ReviewedWriteBaseline>,
        cancel: CancellationToken,
    ) -> ToolOutcome {
        let operation = action.tool_name();

        if cancel.is_cancelled() {
            return ToolOutcome::failure(format!(
                "tool `{operation}` was cancelled before it started"
            ));
        }

        if let Err(error) = validate_action_input(action) {
            return ToolOutcome::failure(error.to_string());
        }

        let sandbox = self.sandbox.clone();
        let mut exec_options = self.exec_options.clone();
        if let Some(timeout) = timeout_override {
            exec_options.timeout = timeout;
            if let Err(error) = exec_options.validate() {
                return ToolOutcome::failure(error.to_string());
            }
        }
        if let Err(error) = self.consume_bound_approval_if_needed(
            action,
            approval.as_ref(),
            expected_binding.as_ref(),
        ) {
            return ToolOutcome::failure(error.to_string());
        }
        let action = action.clone();
        let worker_cancel = cancel.child_token();
        let mut cancellation_guard = ToolCancellationOnDrop::new(worker_cancel.clone());

        let worker = tokio::spawn(async move {
            isolate_tool_dispatch(dispatch_action(
                &sandbox,
                action,
                approval,
                expected_binding,
                exec_options,
                reviewed_write_baseline,
                worker_cancel,
            ))
            .await
        });

        let joined = worker.await;
        cancellation_guard.disarm();

        match joined {
            Ok(Ok(Ok(output))) => ToolOutcome::success(output),
            Ok(Ok(Err(error))) if tool_error_contains_panic(&error) => {
                tracing::error!(
                    tool = operation,
                    error = ?error,
                    "Tool worker panicked"
                );

                unexpected_tool_failure(operation)
            }
            Ok(Ok(Err(error))) => ToolOutcome::failure(error.to_string()),
            Ok(Err(_panic)) => {
                tracing::error!(tool = operation, "Tool dispatch panicked");
                unexpected_tool_failure(operation)
            }
            Err(source) => {
                tracing::error!(
                    tool = operation,
                    error = ?source,
                    "Tool task did not complete normally"
                );

                unexpected_tool_failure(operation)
            }
        }
    }

    fn consume_bound_approval_if_needed(
        &self,
        action: &ToolAction,
        approval: Option<&CommandApproval>,
        expected_binding: Option<&ApprovalBinding>,
    ) -> Result<(), ExecError> {
        let ToolAction::ExecuteCommand {
            command,
            requires_confirmation,
        } = action
        else {
            return Ok(());
        };
        let Some(binding) = expected_binding else {
            return Ok(());
        };
        if !exec::confirmation_decision_with_options(
            command,
            *requires_confirmation,
            &self.exec_options,
        )
        .requires_confirmation()
        {
            return Ok(());
        }
        if !binding.command_digest.matches_command(command)
            || !approval.is_some_and(|approval| {
                approval.permits(command, *requires_confirmation, Some(binding))
            })
        {
            return Ok(());
        }

        let mut registry = self
            .approval_replay_registry
            .lock()
            .map_err(|_| ExecError::ApprovalRegistryUnavailable)?;
        match registry.epoch {
            Some(current) if binding.epoch < current => {
                return Err(ExecError::ApprovalStaleEpoch {
                    supplied: binding.epoch,
                    current,
                });
            }
            Some(current) if binding.epoch > current => {
                registry.epoch = Some(binding.epoch);
                registry.nonces.clear();
            }
            None => registry.epoch = Some(binding.epoch),
            Some(_) => {}
        }

        if registry.nonces.contains(&binding.nonce) {
            return Err(ExecError::ApprovalReplay);
        }
        if registry.nonces.len() >= MAX_CONSUMED_APPROVAL_NONCES {
            return Err(ExecError::ApprovalRegistryFull);
        }
        registry.nonces.insert(binding.nonce);
        Ok(())
    }
}

async fn isolate_tool_dispatch<F>(future: F) -> Result<F::Output, ()>
where
    F: Future,
{
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .map_err(|_| ())
}

struct ToolCancellationOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl ToolCancellationOnDrop {
    const fn new(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ToolCancellationOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

async fn dispatch_action(
    sandbox: &SandboxRoot,
    action: ToolAction,
    approval: Option<CommandApproval>,
    expected_binding: Option<ApprovalBinding>,
    exec_options: ExecOptions,
    reviewed_write_baseline: Option<ReviewedWriteBaseline>,
    cancel: CancellationToken,
) -> Result<String, ToolError> {
    match action {
        ToolAction::ReadFile { path } => {
            fs_ops::read_file_cancellable(sandbox, &path, cancel).await
        }
        ToolAction::ListDirectory { path } => {
            fs_ops::list_directory_cancellable(sandbox, &path, cancel).await
        }
        ToolAction::SearchCode { pattern, path } => {
            search::search_code_cancellable(sandbox, &pattern, path.as_deref(), cancel).await
        }
        ToolAction::ApplyPatch {
            path,
            search,
            replace,
        } => patch::apply_patch_cancellable(sandbox, &path, &search, &replace, cancel).await,
        ToolAction::WriteFile { path, content } => match reviewed_write_baseline {
            Some(baseline) => {
                fs_ops::write_file_reviewed_cancellable(sandbox, &path, &content, baseline, cancel)
                    .await
            }
            None => fs_ops::write_file_cancellable(sandbox, &path, &content, cancel).await,
        },
        ToolAction::ExecuteCommand {
            command,
            requires_confirmation,
        } => exec::execute_command_bound(
            sandbox,
            &command,
            requires_confirmation,
            approval.unwrap_or_default(),
            expected_binding,
            exec_options,
            cancel,
        )
        .await
        .map_err(ToolError::from),
    }
}

fn validate_action_input(action: &ToolAction) -> Result<(), ToolError> {
    match action {
        ToolAction::ReadFile { path }
        | ToolAction::ListDirectory { path }
        | ToolAction::WriteFile { path, .. }
        | ToolAction::ApplyPatch { path, .. } => {
            ensure_input_limit("path", path.len(), MAX_MODEL_PATH_BYTES)?;
        }
        ToolAction::SearchCode { pattern, path } => {
            ensure_input_limit("pattern", pattern.len(), MAX_SEARCH_PATTERN_BYTES)?;
            if let Some(path) = path {
                ensure_input_limit("path", path.len(), MAX_MODEL_PATH_BYTES)?;
            }
        }
        ToolAction::ExecuteCommand { command, .. } => {
            ensure_input_limit("command", command.len(), MAX_COMMAND_BYTES)?;
        }
    }

    match action {
        ToolAction::WriteFile { content, .. } => {
            ensure_input_limit("content", content.len(), MAX_WRITE_FILE_BYTES)?;
        }
        ToolAction::ApplyPatch {
            search, replace, ..
        } => {
            ensure_input_limit("search", search.len(), MAX_PATCH_FILE_BYTES)?;
            ensure_input_limit("replace", replace.len(), MAX_PATCH_RESULT_BYTES)?;
        }
        _ => {}
    }

    Ok(())
}

pub(crate) fn ensure_input_limit(
    field: &'static str,
    actual_bytes: usize,
    limit_bytes: usize,
) -> Result<(), ToolError> {
    if actual_bytes > limit_bytes {
        return Err(ToolError::InputTooLarge {
            field,
            actual_bytes,
            limit_bytes,
        });
    }

    Ok(())
}

pub(crate) fn check_cancellation(
    operation: &'static str,
    requested: &Path,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        return Err(SandboxError::Cancelled {
            operation,
            requested: requested.to_path_buf(),
        }
        .into());
    }

    Ok(())
}

fn tool_error_contains_panic(error: &ToolError) -> bool {
    match error {
        ToolError::WorkerTask { source, .. } => source.is_panic(),
        ToolError::Exec(ExecError::SupervisorTask { source } | ExecError::StdinTask { source }) => {
            source.is_panic()
        }
        ToolError::Exec(ExecError::Capture(exec::CaptureError::ReaderTask { source, .. })) => {
            source.is_panic()
        }
        _ => false,
    }
}

fn unexpected_tool_failure(operation: &'static str) -> ToolOutcome {
    ToolOutcome::failure(format!(
        "tool `{operation}` failed unexpectedly; details were written to the log"
    ))
}

pub(crate) fn reject_excluded_tree(operation: &'static str, path: &Path) -> Result<(), ToolError> {
    if path_has_excluded_component(path) {
        return Err(ToolError::ExcludedPath {
            operation,
            path: path.to_path_buf(),
        });
    }

    Ok(())
}

pub(crate) fn path_has_excluded_component(path: &Path) -> bool {
    path.components().any(|component| {
        let Component::Normal(name) = component else {
            return false;
        };

        name.to_str().is_some_and(|value| {
            value.eq_ignore_ascii_case(".git")
                || value.eq_ignore_ascii_case("target")
                || value.eq_ignore_ascii_case("node_modules")
        })
    })
}

pub(crate) fn sanitize_tool_path(path: &Path) -> String {
    sanitize_tool_text(&path.to_string_lossy())
}

pub(crate) fn sanitize_tool_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '\n' => sanitized.push_str("\\n"),
            '\r' => sanitized.push_str("\\r"),
            '\t' => sanitized.push_str("\\t"),
            '\u{1b}' => sanitized.push_str("\\x1b"),
            control if control.is_control() => {
                sanitized.push_str(&format!("\\u{{{:x}}}", u32::from(control)));
            }
            ordinary => sanitized.push(ordinary),
        }
    }

    sanitized
}

#[cfg(test)]
mod reviewed_write_tests {
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    #[tokio::test]
    async fn async_dispatch_panic_is_contained() {
        let result = isolate_tool_dispatch(async {
            std::panic::resume_unwind(Box::new("tool panic fixture"));
        })
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn trusted_stdin_task_panics_are_treated_as_unexpected_failures() {
        let source = tokio::spawn(async {
            std::panic::resume_unwind(Box::new("stdin panic fixture"));
        })
        .await
        .expect_err("fixture task must panic");
        let error = ToolError::Exec(ExecError::StdinTask { source });

        assert!(tool_error_contains_panic(&error));
    }

    #[tokio::test]
    async fn reviewed_write_refuses_a_destination_changed_after_preview()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        std::fs::write(root.path().join("file.txt"), "reviewed baseline")?;
        let runner = ToolRunner::new(root.path(), Duration::from_secs(5))?;
        let baseline = runner
            .capture_write_file_baseline("file.txt", CancellationToken::new())
            .await?;
        std::fs::write(root.path().join("file.txt"), "manual edit")?;
        let action = ToolAction::WriteFile {
            path: "file.txt".to_owned(),
            content: "agent edit".to_owned(),
        };
        let binding = ApprovalBinding {
            epoch: 1,
            turn_id: 2,
            action_id: 3,
            nonce: ApprovalNonce::new([4; 16]),
            command_digest: CommandDigest::for_command(""),
        };
        let outcome = runner
            .execute_reviewed_write_bound_with_timeout(
                &action,
                binding,
                Duration::from_secs(5),
                baseline,
                CancellationToken::new(),
            )
            .await;

        assert!(matches!(
            outcome,
            ToolOutcome::Failure { ref message } if message.contains("modified concurrently")
        ));
        assert_eq!(
            std::fs::read_to_string(root.path().join("file.txt"))?,
            "manual edit"
        );
        Ok(())
    }
}
