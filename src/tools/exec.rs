use std::{
    collections::{TryReserveError, VecDeque},
    ffi::OsStr,
    fmt,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::Duration,
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::{ChildStderr, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    task::{JoinError, JoinHandle},
};
use tokio_util::sync::CancellationToken;

use super::{SandboxError, SandboxRoot};

#[cfg(not(any(unix, windows)))]
compile_error!("tools::exec currently supports only Unix and Windows process isolation");

pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 100_000;

pub const MAX_EXEC_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_AUTO_APPROVED_TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_TRUSTED_STDIN_BYTES: usize = 64 * 1024;

const PIPE_READ_CHUNK_BYTES: usize = 8 * 1024;
const CAPTURE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const REAP_GRACE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_DEFERRED_REAPERS: usize = 64;

static DEFERRED_REAPERS: OnceLock<StdMutex<Vec<JoinHandle<()>>>> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub confirmation_mode: ShellConfirmationMode,
    strict_allowlist_entries: Vec<StrictAllowlistEntry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShellConfirmationMode {
    #[default]
    Always,
    StrictAllowlist,
}

/// One exact argv vector that may be executed directly when strict allowlisting
/// is enabled. The executable is resolved from a trusted operating-system
/// directory rather than `PATH`, and no shell parses the arguments.
///
/// Configuration code is responsible for offering only semantically read-only
/// commands. This type additionally rejects shells, interpreters, build tools,
/// path-qualified executables, control characters, and non-ASCII argv so those
/// high-risk classes can never become auto-approved through configuration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StrictAllowlistEntry {
    program: String,
    args: Vec<String>,
}

impl StrictAllowlistEntry {
    pub fn new<P, I, A>(program: P, args: I) -> Result<Self, ExecError>
    where
        P: Into<String>,
        I: IntoIterator<Item = A>,
        A: Into<String>,
    {
        let program = program.into();
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        validate_strict_allowlist_entry(&program, &args)?;

        Ok(Self {
            program: program.to_ascii_lowercase(),
            args,
        })
    }

    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    #[must_use]
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl ExecOptions {
    #[must_use]
    pub const fn new(timeout: Duration, max_output_bytes: usize) -> Self {
        Self {
            timeout,
            max_output_bytes,
            confirmation_mode: ShellConfirmationMode::Always,
            strict_allowlist_entries: Vec::new(),
        }
    }

    #[must_use]
    pub const fn with_confirmation_mode(
        mut self,
        confirmation_mode: ShellConfirmationMode,
    ) -> Self {
        self.confirmation_mode = confirmation_mode;
        self
    }

    /// Adds validated exact argv entries to the built-in strict allowlist.
    /// Entries have no effect unless `confirmation_mode` is
    /// [`ShellConfirmationMode::StrictAllowlist`].
    #[must_use]
    pub fn with_strict_allowlist_entries<I>(mut self, entries: I) -> Self
    where
        I: IntoIterator<Item = StrictAllowlistEntry>,
    {
        self.strict_allowlist_entries.extend(entries);
        self
    }

    #[must_use]
    pub fn strict_allowlist_entries(&self) -> &[StrictAllowlistEntry] {
        &self.strict_allowlist_entries
    }

    pub(super) fn validate(&self) -> Result<(), ExecError> {
        if self.timeout.is_zero() {
            return Err(ExecError::InvalidTimeout);
        }

        if self.timeout > MAX_EXEC_TIMEOUT {
            return Err(ExecError::TimeoutTooLarge {
                requested: self.timeout,
                maximum: MAX_EXEC_TIMEOUT,
            });
        }

        if self.max_output_bytes == 0 {
            return Err(ExecError::InvalidOutputLimit);
        }

        if self.max_output_bytes > MAX_OUTPUT_BYTES {
            return Err(ExecError::OutputLimitTooLarge {
                requested: self.max_output_bytes,
                maximum: MAX_OUTPUT_BYTES,
            });
        }

        Ok(())
    }
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_EXEC_TIMEOUT,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            confirmation_mode: ShellConfirmationMode::Always,
            strict_allowlist_entries: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationReason {
    PolicyRequired,
    ModelRequested,
    ForcedRule(&'static str),
    NotAllowlisted,
}

impl fmt::Display for ConfirmationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PolicyRequired => {
                formatter.write_str("project policy requires confirmation for every shell command")
            }
            Self::ModelRequested => formatter.write_str("the model requested confirmation"),
            Self::ForcedRule(rule) => {
                write!(formatter, "the command matched forced-confirm rule: {rule}")
            }
            Self::NotAllowlisted => {
                formatter.write_str("the command is not on the strict auto-confirm allowlist")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationDecision {
    AutoApproved,
    RequiresUserConfirmation { reason: ConfirmationReason },
}

impl ConfirmationDecision {
    #[must_use]
    pub const fn requires_confirmation(self) -> bool {
        matches!(self, Self::RequiresUserConfirmation { .. })
    }

    #[must_use]
    pub const fn reason(self) -> Option<ConfirmationReason> {
        match self {
            Self::AutoApproved => None,
            Self::RequiresUserConfirmation { reason } => Some(reason),
        }
    }
}

/// Подтверждение привязано к точной команде и исходному model-флагу.
///
/// Это предотвращает случайное повторное использование подтверждения для
/// следующего `ExecuteCommand`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApprovalNonce([u8; 16]);

impl ApprovalNonce {
    #[must_use]
    pub const fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommandDigest([u8; 32]);

impl CommandDigest {
    #[must_use]
    pub const fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    #[must_use]
    pub fn for_command(command: &str) -> Self {
        Self(Sha256::digest(command.as_bytes()).into())
    }

    #[must_use]
    pub fn matches_command(self, command: &str) -> bool {
        self == Self::for_command(command)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApprovalBinding {
    pub epoch: u64,
    pub turn_id: u64,
    pub action_id: u64,
    pub nonce: ApprovalNonce,
    pub command_digest: CommandDigest,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum CommandApproval {
    #[default]
    NotGranted,
    Confirmed {
        command: String,
        model_requires_confirmation: bool,
        binding: Option<ApprovalBinding>,
    },
}

impl CommandApproval {
    #[must_use]
    pub fn confirmed_for(command: impl Into<String>, model_requires_confirmation: bool) -> Self {
        Self::Confirmed {
            command: command.into(),
            model_requires_confirmation,
            binding: None,
        }
    }

    #[must_use]
    pub fn confirmed_for_bound(
        command: impl Into<String>,
        model_requires_confirmation: bool,
        binding: ApprovalBinding,
    ) -> Self {
        Self::Confirmed {
            command: command.into(),
            model_requires_confirmation,
            binding: Some(binding),
        }
    }

    pub(crate) fn permits(
        &self,
        command: &str,
        model_requires_confirmation: bool,
        expected_binding: Option<&ApprovalBinding>,
    ) -> bool {
        match self {
            Self::NotGranted => false,
            Self::Confirmed {
                command: approved_command,
                model_requires_confirmation: approved_model_flag,
                binding,
            } => {
                approved_command == command
                    && *approved_model_flag == model_requires_confirmation
                    && expected_binding.is_none_or(|expected| binding.as_ref() == Some(expected))
            }
        }
    }

    pub(crate) const fn binding(&self) -> Option<&ApprovalBinding> {
        match self {
            Self::Confirmed { binding, .. } => binding.as_ref(),
            Self::NotGranted => None,
        }
    }

    const fn is_granted(&self) -> bool {
        matches!(self, Self::Confirmed { .. })
    }
}

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("command must not be empty")]
    EmptyCommand,

    #[error("command contains a NUL byte")]
    NulByte,

    #[error("trusted direct program path must be absolute: {path:?}")]
    DirectProgramNotAbsolute { path: PathBuf },

    #[error("trusted command stdin is {actual_bytes} bytes, exceeding {limit_bytes}")]
    TrustedStdinTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },

    #[error("command is {actual_bytes} bytes, exceeding the permitted {limit_bytes} bytes")]
    CommandTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },

    #[error("command timeout must be greater than zero")]
    InvalidTimeout,

    #[error("command timeout {requested:?} exceeds maximum {maximum:?}")]
    TimeoutTooLarge {
        requested: Duration,
        maximum: Duration,
    },

    #[error("command output limit must be greater than zero")]
    InvalidOutputLimit,

    #[error("command output limit {requested} exceeds maximum {maximum}")]
    OutputLimitTooLarge { requested: usize, maximum: usize },

    #[error("invalid strict allowlist entry for `{program}`: {reason}")]
    InvalidStrictAllowlistEntry {
        program: String,
        reason: &'static str,
    },

    #[error("command requires user confirmation: {reason}")]
    ConfirmationRequired { reason: ConfirmationReason },

    #[error("the supplied confirmation does not match the exact command being run")]
    ApprovalMismatch,

    #[error("the supplied confirmation binding does not match this tool action")]
    ApprovalBindingMismatch,

    #[error("the supplied confirmation nonce has already been consumed")]
    ApprovalReplay,

    #[error("confirmation epoch {supplied} is stale; current epoch is {current}")]
    ApprovalStaleEpoch { supplied: u64, current: u64 },

    #[error("the confirmation replay registry is unavailable")]
    ApprovalRegistryUnavailable,

    #[error("the confirmation replay registry reached its bounded capacity")]
    ApprovalRegistryFull,

    #[error("trusted direct executable `{program}` is unavailable")]
    DirectProgramUnavailable { program: String },

    #[error("workspace identity check failed before command spawn: {source}")]
    WorkspaceIdentity {
        #[source]
        source: SandboxError,
    },

    #[error("Windows SystemRoot is not available")]
    MissingSystemRoot,

    #[error("Windows SystemRoot is not an absolute path: {path:?}")]
    InvalidSystemRoot { path: PathBuf },

    #[error("could not {operation} trusted command shell path {path:?}: {source}")]
    ShellPath {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(
        "resolved command shell {shell:?} is outside trusted system root \
         {system_root:?}"
    )]
    ShellOutsideSystemRoot {
        shell: PathBuf,
        system_root: PathBuf,
    },

    #[error("could not spawn command shell: {source}")]
    Spawn {
        #[source]
        source: io::Error,
    },

    #[error("spawned command did not expose its {stream} pipe")]
    MissingPipe { stream: &'static str },

    #[error("failed to write trusted command input: {source}")]
    Stdin {
        #[source]
        source: io::Error,
    },

    #[error("trusted command input task failed: {source}")]
    StdinTask {
        #[source]
        source: JoinError,
    },

    #[error("failed while waiting for command process: {source}")]
    Wait {
        #[source]
        source: io::Error,
    },

    #[error("failed to terminate command process tree: {source}")]
    Terminate {
        #[source]
        source: io::Error,
    },

    #[error(
        "command process did not become reapable within the cleanup grace \
         period; a background reaper remains active"
    )]
    ReapDeferred,

    #[error("command supervisor task failed: {source}")]
    SupervisorTask {
        #[source]
        source: JoinError,
    },

    #[error(transparent)]
    Capture(#[from] CaptureError),

    #[error("command timed out after {timeout:?}; captured output:\n{output}")]
    TimedOut { timeout: Duration, output: String },

    #[error("command was cancelled; captured output:\n{output}")]
    Cancelled { output: String },

    #[error(
        "command exited unsuccessfully with code {code:?}; \
         captured output:\n{output}"
    )]
    NonZeroExit { code: Option<i32>, output: String },
}

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error(
        "could not reserve {requested} bytes for bounded command output: \
         {source}"
    )]
    Allocation {
        requested: usize,
        #[source]
        source: TryReserveError,
    },

    #[error("failed to read command {stream}: {source}")]
    Read {
        stream: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("command {stream} reader task failed: {source}")]
    ReaderTask {
        stream: &'static str,
        #[source]
        source: JoinError,
    },

    #[error("command output pipes did not close within the capture drain timeout")]
    DrainTimeout,
}

/// Независимое от модели решение harness.
///
/// Приоритет:
///
/// 1. forced-confirm policy;
/// 2. `requires_confirmation=true` модели;
/// 3. строгий allowlist;
/// 4. всё неизвестное требует подтверждения.
#[must_use]
pub fn confirmation_decision(
    command: &str,
    model_requires_confirmation: bool,
) -> ConfirmationDecision {
    confirmation_decision_with_mode(
        command,
        model_requires_confirmation,
        ShellConfirmationMode::Always,
    )
}

#[must_use]
pub fn confirmation_decision_with_mode(
    command: &str,
    model_requires_confirmation: bool,
    mode: ShellConfirmationMode,
) -> ConfirmationDecision {
    confirmation_decision_with_entries(command, model_requires_confirmation, mode, &[])
}

#[must_use]
pub fn confirmation_decision_with_options(
    command: &str,
    model_requires_confirmation: bool,
    options: &ExecOptions,
) -> ConfirmationDecision {
    confirmation_decision_with_entries(
        command,
        model_requires_confirmation,
        options.confirmation_mode,
        options.strict_allowlist_entries(),
    )
}

fn confirmation_decision_with_entries(
    command: &str,
    model_requires_confirmation: bool,
    mode: ShellConfirmationMode,
    entries: &[StrictAllowlistEntry],
) -> ConfirmationDecision {
    if let Some(rule) = forced_confirmation_rule(command) {
        return ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::ForcedRule(rule),
        };
    }

    if model_requires_confirmation {
        return ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::ModelRequested,
        };
    }

    match mode {
        ShellConfirmationMode::Always => ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::PolicyRequired,
        },
        ShellConfirmationMode::StrictAllowlist
            if strict_read_only_invocation(command, entries).is_some() =>
        {
            ConfirmationDecision::AutoApproved
        }
        ShellConfirmationMode::StrictAllowlist => ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::NotAllowlisted,
        },
    }
}

/// Запускает команду в корне `SandboxRoot`.
///
/// `CancellationToken` должен принадлежать конкретному инструментальному
/// вызову. Если future этой функции будет уничтожена снаружи, внутренний
/// supervisor автоматически получит отмену, после чего попытается убить и
/// reap-нуть всё дерево процессов.
pub async fn execute_command(
    sandbox: &SandboxRoot,
    command: &str,
    model_requires_confirmation: bool,
    approval: CommandApproval,
    options: ExecOptions,
    cancel: CancellationToken,
) -> Result<String, ExecError> {
    execute_command_bound(
        sandbox,
        command,
        model_requires_confirmation,
        approval,
        None,
        options,
        cancel,
    )
    .await
}

pub async fn execute_command_bound(
    sandbox: &SandboxRoot,
    command: &str,
    model_requires_confirmation: bool,
    approval: CommandApproval,
    expected_binding: Option<ApprovalBinding>,
    mut options: ExecOptions,
    cancel: CancellationToken,
) -> Result<String, ExecError> {
    options.validate()?;

    if command.len() > super::MAX_COMMAND_BYTES {
        return Err(ExecError::CommandTooLarge {
            actual_bytes: command.len(),
            limit_bytes: super::MAX_COMMAND_BYTES,
        });
    }

    if command.trim().is_empty() {
        return Err(ExecError::EmptyCommand);
    }

    if command.as_bytes().contains(&0) {
        return Err(ExecError::NulByte);
    }

    if expected_binding.is_some_and(|binding| !binding.command_digest.matches_command(command)) {
        return Err(ExecError::ApprovalBindingMismatch);
    }

    let decision =
        confirmation_decision_with_options(command, model_requires_confirmation, &options);

    if let ConfirmationDecision::RequiresUserConfirmation { reason } = decision
        && !approval.permits(
            command,
            model_requires_confirmation,
            expected_binding.as_ref(),
        )
    {
        return if approval.is_granted() {
            if expected_binding.is_some() && approval.binding() != expected_binding.as_ref() {
                Err(ExecError::ApprovalBindingMismatch)
            } else {
                Err(ExecError::ApprovalMismatch)
            }
        } else {
            Err(ExecError::ConfirmationRequired { reason })
        };
    }

    let direct_read_only = if decision == ConfirmationDecision::AutoApproved {
        strict_read_only_invocation(command, options.strict_allowlist_entries())
    } else {
        None
    };
    if direct_read_only.is_some() {
        options.timeout = options.timeout.min(MAX_AUTO_APPROVED_TIMEOUT);
    }

    let sandbox = sandbox.clone();
    let supervisor_cancel = cancel.child_token();

    let mut cancellation_guard = CancellationOnDrop::new(supervisor_cancel.clone());

    let command = command.to_owned();

    let supervisor = tokio::spawn(async move {
        run_supervisor(
            sandbox,
            command,
            options,
            supervisor_cancel,
            direct_read_only,
        )
        .await
    });

    let joined = supervisor.await;
    cancellation_guard.disarm();

    match joined {
        Ok(result) => result,
        Err(source) => Err(ExecError::SupervisorTask { source }),
    }
}

/// Run an exact, user-configured executable without a command shell.
///
/// This path is reserved for trusted local automation such as lifecycle
/// hooks. It inherits the same process-tree cleanup, bounded capture, timeout,
/// workspace identity checks, and secret-free environment as shell tools.
/// Harness permissions are deliberately outside this function: callers may
/// use hook output to deny an action, but never to bypass normal approval.
pub async fn execute_trusted_direct(
    sandbox: &SandboxRoot,
    program: &Path,
    args: &[String],
    stdin: &[u8],
    timeout: Duration,
    max_output_bytes: usize,
    cancel: CancellationToken,
) -> Result<String, ExecError> {
    let options = ExecOptions::new(timeout, max_output_bytes);
    options.validate()?;
    if !program.is_absolute() {
        return Err(ExecError::DirectProgramNotAbsolute {
            path: program.to_path_buf(),
        });
    }
    if stdin.len() > MAX_TRUSTED_STDIN_BYTES {
        return Err(ExecError::TrustedStdinTooLarge {
            actual_bytes: stdin.len(),
            limit_bytes: MAX_TRUSTED_STDIN_BYTES,
        });
    }
    let total_argument_bytes = args
        .iter()
        .try_fold(0_usize, |total, argument| {
            total
                .checked_add(argument.len())
                .and_then(|value| value.checked_add(1))
        })
        .ok_or(ExecError::CommandTooLarge {
            actual_bytes: usize::MAX,
            limit_bytes: super::MAX_COMMAND_BYTES,
        })?;
    if total_argument_bytes > super::MAX_COMMAND_BYTES {
        return Err(ExecError::CommandTooLarge {
            actual_bytes: total_argument_bytes,
            limit_bytes: super::MAX_COMMAND_BYTES,
        });
    }
    if args.iter().any(|argument| argument.as_bytes().contains(&0)) {
        return Err(ExecError::NulByte);
    }

    let sandbox = sandbox.clone();
    let program = program.to_path_buf();
    let args = args.to_vec();
    let stdin = stdin.to_vec();
    let supervisor_cancel = cancel.child_token();
    let mut cancellation_guard = CancellationOnDrop::new(supervisor_cancel.clone());
    let supervisor = tokio::spawn(async move {
        run_trusted_direct_supervisor(sandbox, program, args, stdin, options, supervisor_cancel)
            .await
    });
    let joined = supervisor.await;
    cancellation_guard.disarm();
    match joined {
        Ok(result) => result,
        Err(source) => Err(ExecError::SupervisorTask { source }),
    }
}

async fn run_trusted_direct_supervisor(
    sandbox: SandboxRoot,
    program: PathBuf,
    args: Vec<String>,
    stdin: Vec<u8>,
    options: ExecOptions,
    cancel: CancellationToken,
) -> Result<String, ExecError> {
    if cancel.is_cancelled() {
        return Err(ExecError::Cancelled {
            output: String::new(),
        });
    }
    sandbox
        .verify_ambient_root_identity()
        .map_err(|source| ExecError::WorkspaceIdentity { source })?;
    let project_root = sandbox.ambient_root_path().to_path_buf();
    let mut command = Command::new(program);
    command.args(args);
    configure_command(&mut command, &project_root);
    command.stdin(Stdio::piped());

    sandbox
        .verify_ambient_root_identity()
        .map_err(|source| ExecError::WorkspaceIdentity { source })?;

    let capture = Arc::new(Mutex::new(CombinedCapture::new(options.max_output_bytes)?));
    let mut child = ManagedChild::spawn(&mut command)
        .await
        .map_err(|source| ExecError::Spawn { source })?;
    let child_stdin = child.take_stdin();
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let (child_stdin, stdout, stderr) = match (child_stdin, stdout, stderr) {
        (Some(child_stdin), Some(stdout), Some(stderr)) => (child_stdin, stdout, stderr),
        (child_stdin, stdout, stderr) => {
            let stream = if child_stdin.is_none() {
                "stdin"
            } else if stdout.is_none() {
                "stdout"
            } else {
                "stderr"
            };
            drop(child_stdin);
            drop(stdout);
            drop(stderr);
            terminate_and_reap(child).await?;
            return Err(ExecError::MissingPipe { stream });
        }
    };
    let capture_tasks = CaptureTasks::spawn(stdout, stderr, Arc::clone(&capture));
    let mut stdin_task = tokio::spawn(write_trusted_stdin(child_stdin, stdin));
    let execution_timeout = tokio::time::sleep(options.timeout);
    tokio::pin!(execution_timeout);

    let completion = tokio::select! {
        biased;
        _ = cancel.cancelled() => Completion::Cancelled,
        _ = &mut execution_timeout => Completion::TimedOut,
        wait_result = child.wait() => match wait_result {
            Ok(status) => Completion::Exited(status),
            Err(source) => Completion::WaitFailed(source),
        },
    };

    match completion {
        Completion::Exited(status) => {
            let descendant_cleanup = child.finish_after_exit().await;
            drop(child);
            let stdin_result = finish_stdin_task(&mut stdin_task).await;
            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;
            descendant_cleanup.map_err(|source| ExecError::Terminate { source })?;
            stdin_result?;
            capture_result?;
            if status.success() {
                Ok(output)
            } else {
                Err(ExecError::NonZeroExit {
                    code: status.code(),
                    output,
                })
            }
        }
        Completion::TimedOut => {
            stdin_task.abort();
            let _ = stdin_task.await;
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;
            cleanup_result?;
            if let Err(error) = capture_result {
                tracing::warn!(error = %error, "trusted command capture failed after timeout");
            }
            Err(ExecError::TimedOut {
                timeout: options.timeout,
                output,
            })
        }
        Completion::Cancelled => {
            stdin_task.abort();
            let _ = stdin_task.await;
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;
            cleanup_result?;
            if let Err(error) = capture_result {
                tracing::warn!(error = %error, "trusted command capture failed after cancellation");
            }
            Err(ExecError::Cancelled { output })
        }
        Completion::WaitFailed(source) => {
            stdin_task.abort();
            let _ = stdin_task.await;
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;
            if let Err(cleanup_error) = cleanup_result {
                tracing::error!(
                    wait_error = %source,
                    cleanup_error = %cleanup_error,
                    "trusted command wait and cleanup both failed"
                );
                return Err(cleanup_error);
            }
            if let Err(error) = capture_result {
                tracing::warn!(error = %error, "trusted command capture failed after wait error");
            }
            Err(ExecError::Wait { source })
        }
    }
}

async fn write_trusted_stdin(mut stdin: ChildStdin, content: Vec<u8>) -> io::Result<()> {
    if let Err(source) = stdin.write_all(&content).await
        && source.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(source);
    }
    if let Err(source) = stdin.shutdown().await
        && source.kind() != io::ErrorKind::BrokenPipe
    {
        return Err(source);
    }
    Ok(())
}

async fn finish_stdin_task(task: &mut JoinHandle<io::Result<()>>) -> Result<(), ExecError> {
    match task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(source)) => Err(ExecError::Stdin { source }),
        Err(source) => Err(ExecError::StdinTask { source }),
    }
}

async fn run_supervisor(
    sandbox: SandboxRoot,
    command_line: String,
    options: ExecOptions,
    cancel: CancellationToken,
    direct_read_only: Option<StrictReadOnlyInvocation>,
) -> Result<String, ExecError> {
    let execution_timeout = tokio::time::sleep(options.timeout);
    tokio::pin!(execution_timeout);

    if cancel.is_cancelled() {
        return Err(ExecError::Cancelled {
            output: String::new(),
        });
    }

    let capture = Arc::new(Mutex::new(CombinedCapture::new(options.max_output_bytes)?));

    let project_root = sandbox.ambient_root_path().to_path_buf();
    sandbox
        .verify_ambient_root_identity()
        .map_err(|source| ExecError::WorkspaceIdentity { source })?;
    let mut command = if let Some(invocation) = direct_read_only.as_ref() {
        direct_read_only_command(invocation)?
    } else {
        shell_command(&command_line)?
    };

    configure_command(&mut command, &project_root);

    sandbox
        .verify_ambient_root_identity()
        .map_err(|source| ExecError::WorkspaceIdentity { source })?;

    let mut child = ManagedChild::spawn(&mut command)
        .await
        .map_err(|source| ExecError::Spawn { source })?;

    let stdout = child.take_stdout();
    let stderr = child.take_stderr();

    let missing_stream = if stdout.is_none() {
        Some("stdout")
    } else if stderr.is_none() {
        Some("stderr")
    } else {
        None
    };

    if let Some(stream) = missing_stream {
        drop(stdout);
        drop(stderr);

        terminate_and_reap(child).await?;

        return Err(ExecError::MissingPipe { stream });
    }

    let stdout = match stdout {
        Some(value) => value,
        None => {
            terminate_and_reap(child).await?;

            return Err(ExecError::MissingPipe { stream: "stdout" });
        }
    };

    let stderr = match stderr {
        Some(value) => value,
        None => {
            drop(stdout);
            terminate_and_reap(child).await?;

            return Err(ExecError::MissingPipe { stream: "stderr" });
        }
    };

    let capture_tasks = CaptureTasks::spawn(stdout, stderr, Arc::clone(&capture));

    let completion = tokio::select! {
        biased;

        _ = cancel.cancelled() => Completion::Cancelled,

        _ = &mut execution_timeout => Completion::TimedOut,

        wait_result = child.wait() => {
            match wait_result {
                Ok(status) => Completion::Exited(status),
                Err(source) => Completion::WaitFailed(source),
            }
        }
    };

    match completion {
        Completion::Exited(status) => {
            // Descendants can keep inherited pipes open after their leader exits.
            let descendant_cleanup = child.finish_after_exit().await;
            drop(child);

            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;

            descendant_cleanup.map_err(|source| ExecError::Terminate { source })?;
            capture_result?;

            if status.success() {
                if output.is_empty() {
                    Ok("Command completed successfully with no output".to_owned())
                } else {
                    Ok(output)
                }
            } else {
                Err(ExecError::NonZeroExit {
                    code: status.code(),
                    output,
                })
            }
        }

        Completion::TimedOut => {
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;

            cleanup_result?;

            if let Err(error) = capture_result {
                tracing::warn!(
                    error = %error,
                    "Output capture failed after command timeout"
                );
            }

            Err(ExecError::TimedOut {
                timeout: options.timeout,
                output,
            })
        }

        Completion::Cancelled => {
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;
            let output = render_capture(&capture).await;

            cleanup_result?;

            if let Err(error) = capture_result {
                tracing::warn!(
                    error = %error,
                    "Output capture failed after command cancellation"
                );
            }

            Err(ExecError::Cancelled { output })
        }

        Completion::WaitFailed(source) => {
            let cleanup_result = terminate_and_reap(child).await;
            let capture_result = capture_tasks.finish().await;

            if let Err(cleanup_error) = cleanup_result {
                tracing::error!(
                    wait_error = %source,
                    cleanup_error = %cleanup_error,
                    "Process wait and cleanup both failed"
                );

                return Err(cleanup_error);
            }

            if let Err(capture_error) = capture_result {
                tracing::warn!(
                    error = %capture_error,
                    "Output capture also failed after process wait error"
                );
            }

            Err(ExecError::Wait { source })
        }
    }
}

fn configure_command(command: &mut Command, project_root: &PathBuf) {
    command
        .env_clear()
        .current_dir(project_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .env("CARGO_TERM_COLOR", "never")
        .env("CARGO_TARGET_DIR", project_root.join("target"))
        .env("NO_COLOR", "1")
        .env("TERM", "dumb");

    copy_minimal_environment(command);
    if let Some(path) = sanitized_executable_path(project_root) {
        command.env("PATH", path);
    }
    remove_sensitive_environment(command);
    remove_execution_hook_environment(command);
}

fn remove_sensitive_environment(command: &mut Command) {
    for (name, _) in std::env::vars_os() {
        if is_sensitive_environment_name(&name) {
            command.env_remove(name);
        }
    }

    // Удаляем критический ключ явно, даже если текущий environment iterator
    // по платформенной причине его не вернул.
    command.env_remove("AZURE_OPENAI_API_KEY");
}

fn is_sensitive_environment_name(name: &OsStr) -> bool {
    let uppercase = name.to_string_lossy().to_ascii_uppercase();

    const EXACT_NAMES: &[&str] = &[
        "AZURE_OPENAI_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GOOGLE_APPLICATION_CREDENTIALS",
        "DOCKER_AUTH_CONFIG",
        "KUBECONFIG",
        "SSH_AUTH_SOCK",
        "GPG_AGENT_INFO",
        "CI_JOB_JWT",
    ];

    if EXACT_NAMES.iter().any(|candidate| uppercase == *candidate) {
        return true;
    }

    if uppercase.contains("API_KEY")
        || uppercase.contains("APIKEY")
        || uppercase.contains("PRIVATE_KEY")
        || uppercase.contains("AUTH_TOKEN")
        || uppercase.contains("ACCESS_TOKEN")
    {
        return true;
    }

    uppercase.split(['_', '-']).any(|component| {
        matches!(
            component,
            "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "CREDENTIAL" | "CREDENTIALS"
        )
    })
}

fn remove_execution_hook_environment(command: &mut Command) {
    const HOOK_VARIABLES: &[&str] = &[
        "BASH_ENV",
        "ENV",
        "SHELLOPTS",
        "CDPATH",
        "GLOBIGNORE",
        "PS4",
        "LD_PRELOAD",
        "LD_AUDIT",
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "RUSTC",
        "RUSTDOC",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "GIT_EXTERNAL_DIFF",
        "GIT_ASKPASS",
        "SSH_ASKPASS",
    ];

    for name in HOOK_VARIABLES {
        command.env_remove(name);
    }
}

fn copy_minimal_environment(command: &mut Command) {
    #[cfg(unix)]
    const NAMES: &[&str] = &[
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TMPDIR",
        "CARGO_HOME",
        "RUSTUP_HOME",
    ];
    #[cfg(windows)]
    const NAMES: &[&str] = &[
        "SystemRoot",
        "WINDIR",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "APPDATA",
        "LOCALAPPDATA",
        "PROGRAMDATA",
        "CARGO_HOME",
        "RUSTUP_HOME",
    ];

    for name in NAMES {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }

    #[cfg(windows)]
    command
        .env("PATHEXT", ".COM;.EXE;.BAT;.CMD")
        .env("OS", "Windows_NT");
}

fn sanitized_executable_path(project_root: &Path) -> Option<std::ffi::OsString> {
    std::env::join_paths(trusted_platform_path_directories(project_root)).ok()
}

#[cfg(windows)]
fn trusted_platform_path_directories(project_root: &Path) -> Vec<PathBuf> {
    let canonical_project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut trusted = Vec::new();

    if let Some(system_root) = std::env::var_os("SystemRoot").map(PathBuf::from) {
        for candidate in [
            system_root.join("System32"),
            system_root.clone(),
            system_root.join("System32").join("Wbem"),
            system_root
                .join("System32")
                .join("WindowsPowerShell")
                .join("v1.0"),
        ] {
            push_trusted_path(&mut trusted, &candidate, &canonical_project_root);
        }
    }

    if let Some(base_dirs) = directories::BaseDirs::new() {
        push_trusted_path(
            &mut trusted,
            &base_dirs.home_dir().join(".cargo").join("bin"),
            &canonical_project_root,
        );
    }

    trusted
}

#[cfg(windows)]
fn executable_path_entry(canonical: &Path) -> PathBuf {
    let rendered = canonical.as_os_str().to_string_lossy();
    if let Some(rest) = rendered.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    rendered
        .strip_prefix(r"\\?\")
        .map_or_else(|| canonical.to_path_buf(), PathBuf::from)
}

#[cfg(unix)]
fn trusted_platform_path_directories(project_root: &Path) -> Vec<PathBuf> {
    let canonical_project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let mut trusted = Vec::new();

    for candidate in ["/usr/local/bin", "/usr/bin", "/bin"] {
        push_trusted_path(&mut trusted, Path::new(candidate), &canonical_project_root);
    }

    if let Some(base_dirs) = directories::BaseDirs::new() {
        push_trusted_path(
            &mut trusted,
            &base_dirs.home_dir().join(".cargo").join("bin"),
            &canonical_project_root,
        );
    }

    trusted
}

#[cfg(unix)]
fn executable_path_entry(canonical: &Path) -> PathBuf {
    canonical.to_path_buf()
}

fn push_trusted_path(directories: &mut Vec<PathBuf>, candidate: &Path, project_root: &Path) {
    let Ok(canonical) = std::fs::canonicalize(candidate) else {
        return;
    };
    if !canonical.is_dir() || canonical.starts_with(project_root) {
        return;
    }

    let entry = executable_path_entry(&canonical);
    if !directories.contains(&entry) {
        directories.push(entry);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StrictReadOnlyInvocation {
    program: String,
    args: Vec<String>,
}

fn strict_read_only_invocation(
    command_line: &str,
    configured: &[StrictAllowlistEntry],
) -> Option<StrictReadOnlyInvocation> {
    if command_line.is_empty()
        || command_line != command_line.trim()
        || !command_line.is_ascii()
        || command_line.chars().any(is_shell_control_character)
    {
        return None;
    }

    let mut tokens = command_line.split_ascii_whitespace();
    let raw_program = tokens.next()?;
    let args: Vec<_> = tokens.collect();
    let program = raw_program.to_ascii_lowercase();

    if is_never_auto_approved_program(&program) {
        return None;
    }

    if let Some(entry) = configured.iter().find(|entry| {
        entry.program == program
            && entry
                .args
                .iter()
                .map(String::as_str)
                .eq(args.iter().copied())
    }) {
        return Some(StrictReadOnlyInvocation {
            program: entry.program.clone(),
            args: entry.args.clone(),
        });
    }

    #[cfg(unix)]
    let allowed = matches!(
        (program.as_str(), args.as_slice()),
        ("whoami", [])
            | ("id", [])
            | ("id", ["-u" | "-g"])
            | ("uname", [])
            | ("uname", ["-a" | "-s" | "-r" | "-m"])
    );

    #[cfg(windows)]
    let allowed = matches!(
        (program.as_str(), args.as_slice()),
        ("whoami" | "hostname", [])
    );

    if !allowed {
        return None;
    }

    let program = match program.as_str() {
        #[cfg(unix)]
        "id" => "id",
        #[cfg(unix)]
        "uname" => "uname",
        "whoami" => "whoami",
        #[cfg(windows)]
        "hostname" => "hostname",
        _ => return None,
    };

    Some(StrictReadOnlyInvocation {
        program: program.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
    })
}

fn direct_read_only_command(invocation: &StrictReadOnlyInvocation) -> Result<Command, ExecError> {
    let executable = trusted_read_only_executable(&invocation.program)?;
    let mut command = Command::new(executable);
    command.args(&invocation.args);
    Ok(command)
}

#[cfg(unix)]
fn trusted_read_only_executable(program: &str) -> Result<PathBuf, ExecError> {
    for directory in ["/usr/bin", "/bin"] {
        let trusted_directory = PathBuf::from(directory);
        let canonical_directory = match std::fs::canonicalize(&trusted_directory) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let candidate = trusted_directory.join(program);
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(parent) = canonical.parent() else {
            continue;
        };
        if parent != canonical_directory {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&canonical) else {
            continue;
        };
        if metadata.is_file() {
            return Ok(canonical);
        }
    }

    Err(ExecError::DirectProgramUnavailable {
        program: program.to_owned(),
    })
}

#[cfg(windows)]
fn trusted_read_only_executable(program: &str) -> Result<PathBuf, ExecError> {
    let system_root = std::env::var_os("SystemRoot")
        .ok_or(ExecError::MissingSystemRoot)
        .map(PathBuf::from)?;
    if !system_root.is_absolute() {
        return Err(ExecError::InvalidSystemRoot { path: system_root });
    }

    let canonical_root =
        std::fs::canonicalize(&system_root).map_err(|source| ExecError::ShellPath {
            operation: "canonicalize Windows system root",
            path: system_root,
            source,
        })?;
    let system32 = canonical_root.join("System32");
    let canonical_system32 =
        std::fs::canonicalize(&system32).map_err(|source| ExecError::ShellPath {
            operation: "canonicalize Windows System32",
            path: system32,
            source,
        })?;
    let candidate = canonical_system32.join(format!("{program}.exe"));
    let canonical = std::fs::canonicalize(&candidate).map_err(|source| ExecError::ShellPath {
        operation: "canonicalize trusted direct executable",
        path: candidate,
        source,
    })?;

    if canonical.parent() != Some(canonical_system32.as_path())
        || !canonical.starts_with(&canonical_root)
    {
        return Err(ExecError::ShellOutsideSystemRoot {
            shell: canonical,
            system_root: canonical_root,
        });
    }
    let metadata = std::fs::metadata(&canonical).map_err(|source| ExecError::ShellPath {
        operation: "inspect trusted direct executable",
        path: canonical.clone(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(ExecError::DirectProgramUnavailable {
            program: program.to_owned(),
        });
    }

    Ok(canonical)
}

#[cfg(unix)]
fn shell_command(command_line: &str) -> Result<Command, ExecError> {
    // Абсолютный доверенный путь исключает подмену shell через PATH.
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(command_line);
    Ok(command)
}

#[cfg(windows)]
fn shell_command(command_line: &str) -> Result<Command, ExecError> {
    use std::os::windows::process::CommandExt as _;

    let system_root = std::env::var_os("SystemRoot")
        .ok_or(ExecError::MissingSystemRoot)
        .map(PathBuf::from)?;

    if !system_root.is_absolute() {
        return Err(ExecError::InvalidSystemRoot { path: system_root });
    }

    let canonical_root =
        std::fs::canonicalize(&system_root).map_err(|source| ExecError::ShellPath {
            operation: "canonicalize Windows system root",
            path: system_root.clone(),
            source,
        })?;

    let candidate = canonical_root.join("System32").join("cmd.exe");

    let canonical_shell =
        std::fs::canonicalize(&candidate).map_err(|source| ExecError::ShellPath {
            operation: "canonicalize Windows command shell",
            path: candidate,
            source,
        })?;

    if !canonical_shell.starts_with(&canonical_root) {
        return Err(ExecError::ShellOutsideSystemRoot {
            shell: canonical_shell,
            system_root: canonical_root,
        });
    }

    let metadata = std::fs::metadata(&canonical_shell).map_err(|source| ExecError::ShellPath {
        operation: "inspect Windows command shell",
        path: canonical_shell.clone(),
        source,
    })?;

    if !metadata.is_file() {
        return Err(ExecError::ShellPath {
            operation: "validate Windows command shell",
            path: canonical_shell,
            source: io::Error::new(
                io::ErrorKind::InvalidData,
                "resolved command shell is not a regular file",
            ),
        });
    }

    let mut command = Command::new(canonical_shell);

    command.arg("/D").arg("/S").arg("/C");
    command.as_std_mut().raw_arg(command_line);

    Ok(command)
}

enum Completion {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    WaitFailed(io::Error),
}

struct CancellationOnDrop {
    token: CancellationToken,
    armed: bool,
}

impl CancellationOnDrop {
    const fn new(token: CancellationToken) -> Self {
        Self { token, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.token.cancel();
        }
    }
}

async fn terminate_and_reap(mut child: ManagedChild) -> Result<(), ExecError> {
    let termination_result = child.terminate_tree().await;
    let reap_result = tokio::time::timeout(REAP_GRACE_TIMEOUT, child.wait()).await;

    match reap_result {
        Ok(Ok(_status)) => {
            if let Err(source) = termination_result {
                return Err(ExecError::Terminate { source });
            }

            child.mark_reaped();
            Ok(())
        }

        Ok(Err(source)) => {
            if let Err(termination_error) = termination_result {
                tracing::error!(
                    error = %termination_error,
                    "Process-tree termination also failed before wait error"
                );
            }

            Err(ExecError::Wait { source })
        }

        Err(_) => {
            if let Err(source) = child.terminate_direct() {
                tracing::error!(
                    error = %source,
                    "Fallback direct-child termination failed before \
                     deferred reaping"
                );
            }

            if let Err(source) = termination_result {
                tracing::error!(
                    error = %source,
                    "Process-tree termination failed before deferred reaping"
                );
            }

            spawn_background_reaper(child);

            Err(ExecError::ReapDeferred)
        }
    }
}

fn spawn_background_reaper(mut child: ManagedChild) {
    let reaper = tokio::spawn(async move {
        match child.wait().await {
            Ok(_status) => match child.finish_after_exit().await {
                Ok(()) => {
                    child.mark_reaped();
                    tracing::warn!("Deferred command process reaping completed");
                }
                Err(source) => {
                    tracing::error!(
                        error = %source,
                        "Deferred process-tree cleanup failed"
                    );
                }
            },
            Err(source) => {
                tracing::error!(
                    error = %source,
                    "Deferred command process reaping failed"
                );
            }
        }
    });

    register_deferred_reaper(reaper);
}

fn deferred_reapers() -> &'static StdMutex<Vec<JoinHandle<()>>> {
    DEFERRED_REAPERS.get_or_init(|| StdMutex::new(Vec::new()))
}

fn register_deferred_reaper(reaper: JoinHandle<()>) {
    let mut reapers = deferred_reapers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reapers.retain(|handle| !handle.is_finished());

    if reapers.len() >= MAX_DEFERRED_REAPERS {
        let oldest = reapers.remove(0);
        // Aborting drops ManagedChild. Its process-group/Job-Object guards
        // synchronously request descendant termination.
        oldest.abort();
    }
    reapers.push(reaper);
}

#[must_use]
pub fn deferred_reaper_count() -> usize {
    deferred_reapers()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|handle| !handle.is_finished())
        .count()
}

/// Waits for registered deferred reapers. At the deadline, remaining tasks
/// are aborted so their process-group/Job-Object guards terminate descendants.
/// Returns the number of reapers that had to be force-aborted.
pub async fn drain_deferred_reapers(timeout: Duration) -> usize {
    let handles = {
        let mut reapers = deferred_reapers()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *reapers)
    };
    let now = tokio::time::Instant::now();
    let deadline = now.checked_add(timeout).unwrap_or(now + MAX_EXEC_TIMEOUT);
    let mut handles = handles.into_iter();
    let mut aborted = 0usize;

    while let Some(mut handle) = handles.next() {
        if handle.is_finished() {
            let _ = handle.await;
            continue;
        }

        if tokio::time::timeout_at(deadline, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
            aborted = aborted.saturating_add(1);
            for remaining in handles {
                if !remaining.is_finished() {
                    remaining.abort();
                    aborted = aborted.saturating_add(1);
                }
                let _ = remaining.await;
            }
            break;
        }
    }

    aborted
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputStream {
    Stdout,
    Stderr,
}

impl OutputStream {
    const fn label(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// Общий ограниченный хвост stdout и stderr.
///
/// Pipe полностью вычитываются параллельными задачами, но в памяти хранится
/// только суммарный хвост размером `capacity`.
#[derive(Debug)]
pub struct CombinedCapture {
    bytes: VecDeque<u8>,
    capacity: usize,
    source_bytes_observed: u64,
    truncated: bool,
    last_stream: Option<OutputStream>,
}

impl CombinedCapture {
    pub fn new(capacity: usize) -> Result<Self, CaptureError> {
        let mut bytes = VecDeque::new();

        bytes
            .try_reserve_exact(capacity)
            .map_err(|source| CaptureError::Allocation {
                requested: capacity,
                source,
            })?;

        Ok(Self {
            bytes,
            capacity,
            source_bytes_observed: 0,
            truncated: false,
            last_stream: None,
        })
    }

    #[must_use]
    pub const fn source_bytes_observed(&self) -> u64 {
        self.source_bytes_observed
    }

    #[must_use]
    pub const fn was_truncated(&self) -> bool {
        self.truncated
    }

    fn push(&mut self, stream: OutputStream, bytes: &[u8]) {
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);

        self.source_bytes_observed = self.source_bytes_observed.saturating_add(observed);

        let marker: Option<&'static [u8]> = match (self.last_stream, stream) {
            (None, OutputStream::Stdout)
            | (Some(OutputStream::Stdout), OutputStream::Stdout)
            | (Some(OutputStream::Stderr), OutputStream::Stderr) => None,

            (None, OutputStream::Stderr) => Some(b"--- stderr ---\n"),

            (Some(OutputStream::Stdout), OutputStream::Stderr) => Some(b"\n--- stderr ---\n"),

            (Some(OutputStream::Stderr), OutputStream::Stdout) => Some(b"\n--- stdout ---\n"),
        };

        if let Some(marker_bytes) = marker {
            self.append_bounded(marker_bytes);
        }

        self.append_bounded(bytes);
        self.last_stream = Some(stream);
    }

    fn append_bounded(&mut self, value: &[u8]) {
        if value.is_empty() {
            return;
        }

        if value.len() >= self.capacity {
            let discarded_prefix = value.len().saturating_sub(self.capacity);

            if !self.bytes.is_empty() || discarded_prefix > 0 {
                self.truncated = true;
            }

            self.bytes.clear();
            self.bytes.extend(value[discarded_prefix..].iter().copied());

            return;
        }

        let combined_length = self.bytes.len().saturating_add(value.len());

        if combined_length > self.capacity {
            let remove_count = combined_length.saturating_sub(self.capacity);

            self.bytes.drain(..remove_count);
            self.truncated = true;
        }

        self.bytes.extend(value.iter().copied());
    }

    #[must_use]
    pub fn render(&self) -> String {
        let raw: Vec<u8> = self.bytes.iter().copied().collect();
        let decoded = String::from_utf8_lossy(&raw);
        let sanitized = sanitize_command_output(&decoded);

        let needs_marker = self.truncated || sanitized.len() > self.capacity;

        if !needs_marker {
            return sanitized;
        }

        let marker = format!(
            "[output truncated; observed {} source bytes; showing tail]\n",
            self.source_bytes_observed
        );

        if marker.len() >= self.capacity {
            return utf8_prefix(&marker, self.capacity).to_owned();
        }

        let body_limit = self.capacity.saturating_sub(marker.len());
        let tail = utf8_suffix(&sanitized, body_limit);

        let mut output = String::with_capacity(marker.len().saturating_add(tail.len()));

        output.push_str(&marker);
        output.push_str(tail);
        output
    }
}

async fn render_capture(capture: &Arc<Mutex<CombinedCapture>>) -> String {
    let guard = capture.lock().await;
    guard.render()
}

fn sanitize_command_output(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len().min(MAX_OUTPUT_BYTES));

    let mut characters = value.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push('\t'),

            '\r' => {
                if characters.peek() == Some(&'\n') {
                    let _line_feed = characters.next();
                    sanitized.push('\n');
                } else {
                    sanitized.push_str("\\r");
                }
            }

            '\u{1b}' => sanitized.push_str("\\x1b"),
            '\0' => sanitized.push_str("\\x00"),

            control if control.is_control() => {
                let code = u32::from(control);

                let write_result = if code <= 0xff {
                    write!(sanitized, "\\x{code:02x}")
                } else {
                    write!(sanitized, "\\u{{{code:x}}}")
                };

                if write_result.is_err() {
                    sanitized.push_str("\\u{?}");
                }
            }

            ordinary => sanitized.push(ordinary),
        }
    }

    sanitized
}

fn utf8_prefix(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }

    let mut boundary = max_bytes;

    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_sub(1);
    }

    value.get(..boundary).unwrap_or_default()
}

fn utf8_suffix(value: &str, max_bytes: usize) -> &str {
    if max_bytes == 0 {
        return "";
    }

    if value.len() <= max_bytes {
        return value;
    }

    let mut boundary = value.len().saturating_sub(max_bytes);

    while boundary < value.len() && !value.is_char_boundary(boundary) {
        boundary = boundary.saturating_add(1);
    }

    value.get(boundary..).unwrap_or_default()
}

struct CaptureTasks {
    stdout: JoinHandle<Result<(), CaptureError>>,
    stderr: JoinHandle<Result<(), CaptureError>>,
}

impl CaptureTasks {
    fn spawn(
        stdout: ChildStdout,
        stderr: ChildStderr,
        capture: Arc<Mutex<CombinedCapture>>,
    ) -> Self {
        let stdout = spawn_capture_reader(stdout, OutputStream::Stdout, Arc::clone(&capture));

        let stderr = spawn_capture_reader(stderr, OutputStream::Stderr, capture);

        Self { stdout, stderr }
    }

    async fn finish(self) -> Result<(), CaptureError> {
        let mut stdout = self.stdout;
        let mut stderr = self.stderr;

        let drain_result = tokio::time::timeout(CAPTURE_DRAIN_TIMEOUT, async {
            tokio::join!(&mut stdout, &mut stderr)
        })
        .await;

        match drain_result {
            Ok((stdout_result, stderr_result)) => {
                flatten_reader_result(OutputStream::Stdout, stdout_result)?;

                flatten_reader_result(OutputStream::Stderr, stderr_result)?;

                Ok(())
            }

            Err(_) => {
                stdout.abort();
                stderr.abort();

                let _stdout_result = stdout.await;
                let _stderr_result = stderr.await;

                Err(CaptureError::DrainTimeout)
            }
        }
    }
}

fn spawn_capture_reader<R>(
    reader: R,
    stream: OutputStream,
    capture: Arc<Mutex<CombinedCapture>>,
) -> JoinHandle<Result<(), CaptureError>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(read_pipe(reader, stream, capture))
}

async fn read_pipe<R>(
    mut reader: R,
    stream: OutputStream,
    capture: Arc<Mutex<CombinedCapture>>,
) -> Result<(), CaptureError>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; PIPE_READ_CHUNK_BYTES];

    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|source| CaptureError::Read {
                stream: stream.label(),
                source,
            })?;

        if count == 0 {
            return Ok(());
        }

        let mut guard = capture.lock().await;
        guard.push(stream, &buffer[..count]);
    }
}

fn flatten_reader_result(
    stream: OutputStream,
    result: Result<Result<(), CaptureError>, JoinError>,
) -> Result<(), CaptureError> {
    match result {
        Ok(inner) => inner,
        Err(source) => Err(CaptureError::ReaderTask {
            stream: stream.label(),
            source,
        }),
    }
}

fn forced_confirmation_rule(command: &str) -> Option<&'static str> {
    if command.chars().any(is_shell_control_character) {
        return Some("shell control operator, expansion, quoting, or redirection");
    }

    if contains_dangerous_program(command) {
        return Some("destructive or privileged executable");
    }

    if is_dangerous_git_operation(command) {
        return Some("destructive or remote-mutating git operation");
    }

    None
}

fn validate_strict_allowlist_entry(program: &str, args: &[String]) -> Result<(), ExecError> {
    let invalid = |reason| ExecError::InvalidStrictAllowlistEntry {
        program: program.to_owned(),
        reason,
    };

    if program.is_empty() || !program.is_ascii() {
        return Err(invalid("program must be non-empty ASCII"));
    }
    if program.chars().any(|character| {
        character.is_ascii_whitespace()
            || is_shell_control_character(character)
            || matches!(character, '/' | '\\')
    }) {
        return Err(invalid("program must be an unqualified executable name"));
    }
    if is_never_auto_approved_program(&program.to_ascii_lowercase()) {
        return Err(invalid(
            "shells, interpreters, and build tools cannot be auto-approved",
        ));
    }
    if args.len() > 64 {
        return Err(invalid("argv contains more than 64 arguments"));
    }
    let mut total_bytes = program.len();
    for arg in args {
        total_bytes = total_bytes.saturating_add(1).saturating_add(arg.len());
        if arg.is_empty()
            || !arg.is_ascii()
            || arg.chars().any(|character| {
                character.is_ascii_whitespace() || is_shell_control_character(character)
            })
        {
            return Err(invalid(
                "arguments must be non-empty ASCII tokens without shell controls",
            ));
        }
    }
    if total_bytes > super::MAX_COMMAND_BYTES {
        return Err(invalid("argv exceeds the command input limit"));
    }
    if !is_configurable_read_only_entry(&program.to_ascii_lowercase(), args) {
        return Err(invalid(
            "entry is not in the fixed read-only direct-exec catalog",
        ));
    }

    Ok(())
}

fn is_configurable_read_only_entry(program: &str, args: &[String]) -> bool {
    let args: Vec<_> = args.iter().map(String::as_str).collect();

    #[cfg(unix)]
    {
        matches!(
            (program, args.as_slice()),
            ("whoami" | "hostname" | "pwd" | "date", [])
                | ("id", [] | ["-u" | "-g"])
                | ("uname", [] | ["-a" | "-s" | "-r" | "-m" | "-n"])
                | ("ls", [] | ["-a"] | ["-l"] | ["-la"] | ["-al"])
        )
    }

    #[cfg(windows)]
    {
        matches!(
            (program, args.as_slice()),
            (
                "whoami" | "hostname" | "ipconfig" | "tasklist" | "systeminfo",
                []
            ) | ("whoami", ["/user" | "/groups"])
                | ("ipconfig", ["/all"])
        )
    }
}

fn is_never_auto_approved_program(program: &str) -> bool {
    let program = program_basename(program).to_ascii_lowercase();
    let program = program.strip_suffix(".exe").unwrap_or(&program);

    matches!(
        program,
        "cargo"
            | "rustc"
            | "rustdoc"
            | "make"
            | "gmake"
            | "cmake"
            | "ctest"
            | "ninja"
            | "meson"
            | "msbuild"
            | "devenv"
            | "dotnet"
            | "gradle"
            | "gradlew"
            | "mvn"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "sh"
            | "bash"
            | "dash"
            | "zsh"
            | "fish"
            | "cmd"
            | "command"
            | "powershell"
            | "pwsh"
            | "python"
            | "python3"
            | "perl"
            | "ruby"
            | "node"
            | "deno"
            | "env"
            | "xargs"
            | "find"
    )
}

fn is_shell_control_character(character: char) -> bool {
    matches!(
        character,
        '|' | ';'
            | '&'
            | '>'
            | '<'
            | '`'
            | '$'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '*'
            | '?'
            | '~'
            | '!'
            | '^'
            | '%'
            | '\''
            | '"'
            | '\\'
            | '\n'
            | '\r'
            | '\0'
    )
}

fn contains_dangerous_program(command: &str) -> bool {
    command
        .split_ascii_whitespace()
        .map(program_basename)
        .any(|program| {
            matches!(
                program,
                "rm" | "rmdir"
                    | "sudo"
                    | "doas"
                    | "su"
                    | "del"
                    | "erase"
                    | "format"
                    | "mkfs"
                    | "dd"
                    | "shutdown"
                    | "reboot"
                    | "poweroff"
                    | "halt"
                    | "chmod"
                    | "chown"
                    | "taskkill"
                    | "powershell"
                    | "powershell.exe"
                    | "pwsh"
                    | "pwsh.exe"
            )
        })
}

fn program_basename(token: &str) -> &str {
    let stripped = token.trim_matches(|character| {
        matches!(character, '\'' | '"' | ',' | ':' | '[' | ']' | '{' | '}')
    });

    match stripped.rsplit(['/', '\\']).next() {
        Some(name) => name,
        None => stripped,
    }
}

fn is_dangerous_git_operation(command: &str) -> bool {
    let mut words = command.split_ascii_whitespace();

    let Some(program) = words.next() else {
        return false;
    };

    if program_basename(program) != "git" {
        return false;
    }

    let Some(subcommand) = words.next() else {
        return false;
    };

    match subcommand {
        "push" | "clean" => true,

        "reset" => words.any(|word| word == "--hard"),

        "checkout" => words.any(|word| word == "-f" || word == "--force"),

        _ => false,
    }
}

fn combine_termination_errors(primary: io::Error, fallback: io::Error) -> io::Error {
    let kind = primary.kind();

    io::Error::new(
        kind,
        format!(
            "process-tree termination failed: {primary}; \
             direct-child fallback also failed: {fallback}"
        ),
    )
}

#[cfg(unix)]
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};

#[cfg(unix)]
use std::os::unix::process::CommandExt as _;

#[cfg(unix)]
struct ManagedChild {
    child: tokio::process::Child,
    process_group: ProcessGroupGuard,
}

#[cfg(unix)]
impl ManagedChild {
    async fn spawn(command: &mut Command) -> io::Result<Self> {
        command.as_std_mut().process_group(0);

        let child = command.spawn()?;

        let process_id = match child.id() {
            Some(value) => value,
            None => {
                dispose_unidentified_child(child).await;

                return Err(io::Error::other(
                    "spawned child did not expose a process id",
                ));
            }
        };

        let raw_process_id = match i32::try_from(process_id) {
            Ok(value) if value > 0 => value,
            Ok(_) => {
                dispose_unidentified_child(child).await;

                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "child process id must be positive",
                ));
            }
            Err(_) => {
                dispose_unidentified_child(child).await;

                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "child process id does not fit into pid_t",
                ));
            }
        };

        Ok(Self {
            child,
            process_group: ProcessGroupGuard {
                process_group_id: Pid::from_raw(raw_process_id),
                armed: true,
            },
        })
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    async fn terminate_tree(&mut self) -> io::Result<()> {
        match self.process_group.send_sigkill() {
            Ok(GroupSignalResult::Signalled) => Ok(()),

            Ok(GroupSignalResult::NoSuchGroup) => self.terminate_direct(),

            Err(group_error) => match self.terminate_direct() {
                Ok(()) => Err(group_error),
                Err(direct_error) => Err(combine_termination_errors(group_error, direct_error)),
            },
        }
    }

    async fn finish_after_exit(&mut self) -> io::Result<()> {
        match self.process_group.send_sigkill() {
            Ok(GroupSignalResult::Signalled | GroupSignalResult::NoSuchGroup) => {
                self.process_group.armed = false;
                Ok(())
            }
            Err(source) => Err(source),
        }
    }

    fn terminate_direct(&mut self) -> io::Result<()> {
        match self.child.start_kill() {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(source) => Err(source),
        }
    }

    fn mark_reaped(&mut self) {
        self.process_group.armed = false;
    }
}

#[cfg(unix)]
async fn dispose_unidentified_child(mut child: tokio::process::Child) {
    match child.start_kill() {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::InvalidInput => {}
        Err(source) => {
            tracing::error!(
                error = %source,
                "Could not terminate child without process id"
            );
        }
    }

    match tokio::time::timeout(REAP_GRACE_TIMEOUT, child.wait()).await {
        Ok(Ok(_status)) => {}
        Ok(Err(source)) => {
            tracing::error!(
                error = %source,
                "Could not reap child without process id"
            );
        }
        Err(_) => {
            let reaper = tokio::spawn(async move {
                if let Err(source) = child.wait().await {
                    tracing::error!(
                        error = %source,
                        "Deferred reaping failed for child without process id"
                    );
                }
            });

            register_deferred_reaper(reaper);
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GroupSignalResult {
    Signalled,
    NoSuchGroup,
}

#[cfg(unix)]
struct ProcessGroupGuard {
    process_group_id: Pid,
    armed: bool,
}

#[cfg(unix)]
impl ProcessGroupGuard {
    fn send_sigkill(&self) -> io::Result<GroupSignalResult> {
        match killpg(self.process_group_id, Signal::SIGKILL) {
            Ok(()) => Ok(GroupSignalResult::Signalled),
            Err(Errno::ESRCH) => Ok(GroupSignalResult::NoSuchGroup),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        match self.send_sigkill() {
            Ok(_) => {}
            Err(source) => {
                tracing::error!(
                    process_group_id = self.process_group_id.as_raw(),
                    error = %source,
                    "Could not kill process group from drop guard"
                );
            }
        }
    }
}

#[cfg(windows)]
use command_group::{AsyncCommandGroup as _, AsyncGroupChild};

#[cfg(windows)]
struct ManagedChild {
    child: AsyncGroupChild,
}

#[cfg(windows)]
impl ManagedChild {
    async fn spawn(command: &mut Command) -> io::Result<Self> {
        // The Job Object kills descendants even if the shell exits first.
        let child = command.group_spawn()?;
        Ok(Self { child })
    }

    fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.inner().stdout.take()
    }

    fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.inner().stdin.take()
    }

    fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.inner().stderr.take()
    }

    async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait().await
    }

    async fn terminate_tree(&mut self) -> io::Result<()> {
        match self.child.kill().await {
            Ok(()) => Ok(()),

            Err(job_error) => match self.terminate_direct() {
                Ok(()) => Err(job_error),
                Err(direct_error) => Err(combine_termination_errors(job_error, direct_error)),
            },
        }
    }

    async fn finish_after_exit(&mut self) -> io::Result<()> {
        Ok(())
    }

    fn terminate_direct(&mut self) -> io::Result<()> {
        match self.child.inner().start_kill() {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::InvalidInput => Ok(()),
            Err(source) => Err(source),
        }
    }

    fn mark_reaped(&mut self) {
        // The Job Object remains the final RAII cleanup guard.
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, time::Duration};

    use super::{
        deferred_reaper_count, drain_deferred_reapers, is_sensitive_environment_name,
        register_deferred_reaper,
    };

    #[test]
    fn recognizes_sensitive_environment_names() {
        assert!(is_sensitive_environment_name(OsStr::new(
            "AZURE_OPENAI_API_KEY"
        )));
        assert!(is_sensitive_environment_name(OsStr::new(
            "CARGO_REGISTRIES_PRIVATE_TOKEN"
        )));
        assert!(is_sensitive_environment_name(OsStr::new(
            "DATABASE_PASSWORD"
        )));
        assert!(is_sensitive_environment_name(OsStr::new(
            "AWS_SECRET_ACCESS_KEY"
        )));

        assert!(!is_sensitive_environment_name(OsStr::new("PATH")));
        assert!(!is_sensitive_environment_name(OsStr::new(
            "CARGO_MANIFEST_DIR"
        )));
    }

    #[tokio::test]
    async fn deferred_reaper_registry_aborts_at_shutdown_deadline() {
        let _previously_aborted = drain_deferred_reapers(Duration::ZERO).await;
        register_deferred_reaper(tokio::spawn(async {
            std::future::pending::<()>().await;
        }));

        assert_eq!(deferred_reaper_count(), 1);
        assert_eq!(drain_deferred_reapers(Duration::from_millis(10)).await, 1);
        assert_eq!(deferred_reaper_count(), 0);
    }
}
