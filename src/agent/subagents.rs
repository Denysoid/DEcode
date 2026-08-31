use std::{
    collections::BTreeMap,
    io::Write as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use thiserror::Error;
use tokio::{
    io::AsyncWriteExt,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::{
    api::{
        FunctionCall, FunctionToolDefinition, InputMessage, ReasoningEffort, ResponsesClient,
        ResponsesRequest,
    },
    config::{AgentConfig, ApiConfig},
    error::ApiError,
    mcp::{McpCallOutput, McpConfig, McpManager, McpPermissionDecision},
    notice::UiNotice,
    parser::{ParserEvent, ToolAction, ToolOutcome, parse_turn, visible_assistant_text},
    privacy::PrivacyShield,
    tools::{
        CommandApproval, ConfirmationDecision, ConfirmationReason, DEFAULT_MAX_OUTPUT_BYTES,
        ExecOptions, PatchReview, ToolRunner,
    },
};

use super::{
    orchestrator::UiSnapshot,
    profiles::{AgentProfile, AgentProfileCatalog, AgentProfileCatalogSnapshot, AgentProfileError},
    scheduler::{
        DependencyDecision, DependencyState, ScheduleError, dependency_decision,
        file_claims_cover_path, normalize_dependencies, normalize_file_claims,
        validate_dependency_graph, writer_claims_conflict,
    },
    side_chat::has_visible_text,
    worktree::{ManagedWorktree, WorktreeChangeSet, WorktreeError, WorktreeManager},
};

const MAX_TASK_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 128 * 1024;
const MAX_TRANSCRIPT_ENTRIES: usize = 128;
const MAX_TRANSCRIPT_ENTRY_BYTES: usize = 16 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const JOURNAL_COMPACT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_RECOVERY_STATE_BYTES: usize = 2 * 1024 * 1024;
const MAX_DEPENDENCY_HANDOFF_BYTES: usize = 64 * 1024;
const CHILD_WAIT_MAX: Duration = Duration::from_secs(30);
const TOKEN_BUDGET_INCREMENT: u64 = 50_000;
const SPAWN_CHILD_TOOL: &str = "spawn_agent";
const LIST_CHILDREN_TOOL: &str = "list_agents";
const GET_CHILD_TOOL: &str = "get_agent";
const MESSAGE_CHILD_TOOL: &str = "send_agent_message";
const INTERRUPT_CHILD_TOOL: &str = "interrupt_agent";
const WAIT_CHILD_TOOL: &str = "wait_agent";

#[derive(Debug, Error)]
pub enum SubagentError {
    #[error("sub-agents are disabled")]
    Disabled,
    #[error("sub-agent worktree service is unavailable: {0}")]
    Unavailable(String),
    #[error("sub-agent task must not be blank")]
    EmptyTask,
    #[error("sub-agent task is {actual_bytes} bytes, exceeding {limit_bytes}")]
    TaskTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
    #[error("sub-agent message must not be blank")]
    EmptyMessage,
    #[error("sub-agent message is {actual_bytes} bytes, exceeding {limit_bytes}")]
    MessageTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
    #[error("sub-agent limit reached for this session ({limit})")]
    SessionLimit { limit: u16 },
    #[error("recursive sub-agent depth limit reached ({limit})")]
    DepthLimit { limit: u8 },
    #[error("sub-agent {parent} already has the configured maximum of {limit} children")]
    ChildLimit { parent: SubagentId, limit: u16 },
    #[error("sub-agent {id} still has descendants requiring completion, recovery, or review")]
    DescendantsPending { id: SubagentId },
    #[error("sub-agent {id} is outside the caller's descendant subtree")]
    DescendantAccess { id: SubagentId },
    #[error("unknown sub-agent {0}")]
    Unknown(SubagentId),
    #[error("sub-agent {id} changed since the UI action was rendered")]
    Stale { id: SubagentId },
    #[error("sub-agent {id} is not running")]
    NotRunning { id: SubagentId },
    #[error("sub-agent {id} has no resumable writer checkpoint")]
    NotRecoverable { id: SubagentId },
    #[error("sub-agent {id} has no pending command approval")]
    NoPendingApproval { id: SubagentId },
    #[error("sub-agent {id} has no pending token-budget decision")]
    NoPendingBudgetDecision { id: SubagentId },
    #[error("sub-agent {id} has no pending file changes")]
    NoPendingChanges { id: SubagentId },
    #[error("sub-agent {id} change set no longer matches digest {digest}")]
    StaleChanges { id: SubagentId, digest: String },
    #[error("sub-agent {id} has no pending change for {path:?}")]
    UnknownChange { id: SubagentId, path: String },
    #[error("sub-agent coordinator state lock was poisoned")]
    StatePoisoned,
    #[error("sub-agent profile catalog lock was poisoned")]
    ProfileStatePoisoned,
    #[error("sub-agent profile reload task failed: {0}")]
    ProfileReloadTask(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Profile(#[from] AgentProfileError),
    #[error(transparent)]
    Schedule(#[from] ScheduleError),
    #[error("dependency agents did not complete successfully: {ids:?}")]
    DependenciesFailed { ids: Vec<SubagentId> },
    #[error("dependency agent {id} belongs to another session")]
    DependencyOwnership { id: SubagentId },
    #[error("writer changed files outside its declared file claims: {paths:?}")]
    FileClaimViolation { paths: Vec<String> },
    #[error("sub-agent worker registry lock was poisoned")]
    WorkerRegistryPoisoned,
    #[error("API error: {0}")]
    Api(#[from] ApiError),
    #[error("MCP error: {0}")]
    Mcp(#[from] crate::mcp::McpError),
    #[error("worktree error: {0}")]
    Worktree(#[from] WorktreeError),
    #[error("tool runner could not start: {0}")]
    ToolRunner(String),
    #[error("sub-agent response protocol error: {0}")]
    Protocol(String),
    #[error("sub-agent context exceeded its configured budget")]
    ContextBudget,
    #[error("sub-agent {scope} token budget exhausted ({used}/{limit} tokens used or reserved)")]
    TokenBudgetExhausted {
        scope: SubagentBudgetScope,
        used: u64,
        limit: u64,
    },
    #[error("sub-agent journal I/O error at {path:?}: {source}")]
    PersistenceIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("sub-agent journal serialization failed: {0}")]
    PersistenceFormat(#[from] serde_json::Error),
    #[error("sub-agent journal writer is unavailable")]
    PersistenceUnavailable,
    #[error("sub-agent journal durability acknowledgement failed")]
    PersistenceAcknowledgement,
    #[error("sub-agent recovery state is {actual_bytes} bytes, exceeding {limit_bytes}")]
    RecoveryStateTooLarge {
        actual_bytes: usize,
        limit_bytes: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubagentId(u64);

impl SubagentId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for SubagentId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "agent-{:04}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMode {
    Research,
    Writer,
}

impl std::fmt::Display for SubagentMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Research => "read-only",
            Self::Writer => "isolated writer",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Queued,
    WaitingDependencies,
    Starting,
    Running,
    WaitingApproval,
    WaitingBudget,
    Cancelling,
    RecoveryRequired,
    ReadyForReview,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
    DependencyFailed,
}

impl SubagentStatus {
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::WaitingDependencies
                | Self::Starting
                | Self::Running
                | Self::WaitingApproval
                | Self::WaitingBudget
                | Self::Cancelling
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::ReadyForReview
                | Self::Completed
                | Self::Failed
                | Self::Cancelled
                | Self::TimedOut
                | Self::Interrupted
                | Self::DependencyFailed
        )
    }

    #[must_use]
    pub const fn is_recoverable(self) -> bool {
        matches!(self, Self::RecoveryRequired)
    }
}

impl std::fmt::Display for SubagentStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queued => "queued",
            Self::WaitingDependencies => "waiting for dependencies",
            Self::Starting => "starting",
            Self::Running => "working",
            Self::WaitingApproval => "waiting for approval",
            Self::WaitingBudget => "waiting for token budget",
            Self::Cancelling => "stopping",
            Self::RecoveryRequired => "recovery required",
            Self::ReadyForReview => "ready for review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed out",
            Self::Interrupted => "interrupted after restart",
            Self::DependencyFailed => "dependency failed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRecoveryAction {
    pub action_id: u64,
    pub action: ToolAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRecoverySummary {
    pub attempt: u32,
    pub checkpoint_at: DateTime<Utc>,
    pub reason: String,
    pub uncertain_action: Option<SubagentRecoveryAction>,
    pub can_resume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentPendingCommand {
    pub action_id: u64,
    pub command: String,
    pub model_requested_confirmation: bool,
    pub mcp: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentBudgetScope {
    Agent,
    SessionTree,
}

impl std::fmt::Display for SubagentBudgetScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Agent => "worker",
            Self::SessionTree => "session tree",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentPendingBudget {
    pub scope: SubagentBudgetScope,
    pub used: u64,
    pub limit: u64,
    pub suggested_increase: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentTranscriptEntry {
    pub at: DateTime<Utc>,
    pub label: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct SubagentSnapshot {
    pub id: SubagentId,
    pub parent_id: Option<SubagentId>,
    pub depth: u8,
    pub revision: u64,
    pub session_id: Option<String>,
    pub label: String,
    pub task: String,
    pub profile_id: String,
    pub profile_name: String,
    pub mode: SubagentMode,
    pub status: SubagentStatus,
    pub deployment: String,
    pub reasoning_effort: ReasoningEffort,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub token_budget: u64,
    pub tool_iterations: u32,
    pub last_message: String,
    pub result: String,
    pub error: Option<String>,
    pub worktree: Option<String>,
    pub base_commit: Option<String>,
    pub changed_files: Arc<[String]>,
    pub resolved_files: Arc<[String]>,
    pub change_digest: Option<String>,
    pub pending_command: Option<SubagentPendingCommand>,
    pub pending_budget: Option<SubagentPendingBudget>,
    pub transcript: Arc<[SubagentTranscriptEntry]>,
    pub recovery: Option<SubagentRecoverySummary>,
    pub dependencies: Arc<[SubagentId]>,
    pub file_claims: Arc<[String]>,
}

#[derive(Debug, Clone)]
pub struct SubagentFleetSnapshot {
    pub revision: u64,
    pub enabled: bool,
    pub capacity: u16,
    pub active: usize,
    pub total_tokens: u64,
    pub token_budget: u64,
    pub availability_error: Option<String>,
    pub mcp_enabled: bool,
    pub mcp_status: UiNotice,
    pub profiles: AgentProfileCatalogSnapshot,
    pub agents: Arc<[SubagentSnapshot]>,
}

impl Default for SubagentFleetSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            enabled: false,
            capacity: 0,
            active: 0,
            total_tokens: 0,
            token_budget: 0,
            availability_error: None,
            mcp_enabled: false,
            mcp_status: crate::notice::UiNotice::SubagentMcpDisabled,
            profiles: AgentProfileCatalogSnapshot::default(),
            agents: Arc::from([]),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SpawnSubagentRequest {
    pub task: String,
    pub profile_id: String,
    pub session_id: Option<String>,
    pub deployment: String,
    pub reasoning_effort: ReasoningEffort,
    pub instructions: String,
    pub dependencies: Vec<SubagentId>,
    pub file_claims: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedSpawnSubagentRequest {
    task: String,
    profile: AgentProfile,
    session_id: Option<String>,
    deployment: String,
    reasoning_effort: ReasoningEffort,
    instructions: String,
    dependencies: Arc<[SubagentId]>,
    file_claims: Arc<[String]>,
    parent_id: Option<SubagentId>,
    depth: u8,
    workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SubagentFileReview {
    pub agent_id: SubagentId,
    pub agent_revision: u64,
    pub change_digest: String,
    pub path: String,
    pub binary: bool,
    pub review: Option<Arc<PatchReview>>,
}

#[derive(Debug, Clone)]
pub enum SubagentFileDecision {
    TextHunks(Vec<bool>),
    ApproveBinary,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RecoveryState {
    replay: Vec<Value>,
    next_action_id: u64,
    attempt: u32,
    checkpoint_at: DateTime<Utc>,
    pending_action: Option<SubagentRecoveryAction>,
    reason: String,
    #[serde(default)]
    allow_fresh_worktree: bool,
    #[serde(default)]
    dependency_context_added: bool,
}

impl RecoveryState {
    fn summary(&self, has_worktree: bool) -> SubagentRecoverySummary {
        SubagentRecoverySummary {
            attempt: self.attempt,
            checkpoint_at: self.checkpoint_at,
            reason: self.reason.clone(),
            uncertain_action: self.pending_action.clone(),
            can_resume: self.can_resume(has_worktree),
        }
    }

    const fn can_resume(&self, has_worktree: bool) -> bool {
        has_worktree || (self.allow_fresh_worktree && self.pending_action.is_none())
    }
}

struct PersistenceUpdate {
    record: PersistedAgent,
    acknowledgement: Option<oneshot::Sender<Result<(), PersistenceAckError>>>,
}

#[derive(Debug, Error)]
#[error("sub-agent journal sync failed: {message}")]
struct PersistenceAckError {
    message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedAgent {
    id: SubagentId,
    #[serde(default)]
    parent_id: Option<SubagentId>,
    #[serde(default = "default_subagent_depth")]
    depth: u8,
    revision: u64,
    session_id: Option<String>,
    label: String,
    task: String,
    #[serde(default)]
    profile_id: String,
    #[serde(default)]
    profile_name: String,
    mode: SubagentMode,
    status: SubagentStatus,
    deployment: String,
    reasoning_effort: ReasoningEffort,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    completed_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    #[serde(default)]
    token_budget: u64,
    tool_iterations: u32,
    last_message: String,
    result: String,
    error: Option<String>,
    base_commit: Option<String>,
    changed_files: Vec<String>,
    #[serde(default)]
    resolved_files: Vec<String>,
    change_digest: Option<String>,
    #[serde(default)]
    transcript: Vec<SubagentTranscriptEntry>,
    #[serde(default)]
    recovery: Option<RecoveryState>,
    #[serde(default)]
    dependencies: Vec<SubagentId>,
    #[serde(default)]
    file_claims: Vec<String>,
}

impl PersistedAgent {
    fn from_snapshot(snapshot: &SubagentSnapshot, recovery: Option<RecoveryState>) -> Self {
        Self {
            id: snapshot.id,
            parent_id: snapshot.parent_id,
            depth: snapshot.depth,
            revision: snapshot.revision,
            session_id: snapshot.session_id.clone(),
            label: snapshot.label.clone(),
            task: snapshot.task.clone(),
            profile_id: snapshot.profile_id.clone(),
            profile_name: snapshot.profile_name.clone(),
            mode: snapshot.mode,
            status: snapshot.status,
            deployment: snapshot.deployment.clone(),
            reasoning_effort: snapshot.reasoning_effort,
            created_at: snapshot.created_at,
            started_at: snapshot.started_at,
            completed_at: snapshot.completed_at,
            updated_at: snapshot.updated_at,
            input_tokens: snapshot.input_tokens,
            output_tokens: snapshot.output_tokens,
            total_tokens: snapshot.total_tokens,
            token_budget: snapshot.token_budget,
            tool_iterations: snapshot.tool_iterations,
            last_message: snapshot.last_message.clone(),
            result: snapshot.result.clone(),
            error: snapshot.error.clone(),
            base_commit: snapshot.base_commit.clone(),
            changed_files: snapshot.changed_files.to_vec(),
            resolved_files: snapshot.resolved_files.to_vec(),
            change_digest: snapshot.change_digest.clone(),
            transcript: snapshot.transcript.to_vec(),
            recovery,
            dependencies: snapshot.dependencies.to_vec(),
            file_claims: snapshot.file_claims.to_vec(),
        }
    }

    fn into_snapshot(self) -> SubagentSnapshot {
        let profile_id = if self.profile_id.is_empty() {
            built_in_profile_id(self.mode).to_owned()
        } else {
            self.profile_id
        };
        let profile_name = if self.profile_name.is_empty() {
            built_in_profile_name(self.mode).to_owned()
        } else {
            self.profile_name
        };
        SubagentSnapshot {
            id: self.id,
            parent_id: self.parent_id,
            depth: self.depth,
            revision: self.revision,
            session_id: self.session_id,
            label: self.label,
            task: self.task,
            profile_id,
            profile_name,
            mode: self.mode,
            status: self.status,
            deployment: self.deployment,
            reasoning_effort: self.reasoning_effort,
            created_at: self.created_at,
            started_at: self.started_at,
            completed_at: self.completed_at,
            updated_at: self.updated_at,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            token_budget: self.token_budget,
            tool_iterations: self.tool_iterations,
            last_message: self.last_message,
            result: self.result,
            error: self.error,
            worktree: None,
            base_commit: self.base_commit,
            changed_files: Arc::from(self.changed_files),
            resolved_files: Arc::from(self.resolved_files),
            change_digest: self.change_digest,
            pending_command: None,
            pending_budget: None,
            transcript: Arc::from(self.transcript),
            recovery: self.recovery.as_ref().map(|state| state.summary(false)),
            dependencies: Arc::from(self.dependencies),
            file_claims: Arc::from(self.file_claims),
        }
    }
}

const fn default_subagent_depth() -> u8 {
    1
}

struct AgentRecord {
    snapshot: SubagentSnapshot,
    cancel: CancellationToken,
    messages: mpsc::Sender<String>,
    approval: Option<oneshot::Sender<bool>>,
    budget_approval: Option<oneshot::Sender<bool>>,
    restart_recovery: Arc<AtomicBool>,
    worktree: Option<ManagedWorktree>,
    changes: Option<WorktreeChangeSet>,
    recovery: Option<RecoveryState>,
    schedule_reserved: bool,
    /// Capacity held by an in-flight provider request. This is deliberately
    /// not persisted: no request survives process restart, and restored work
    /// must reserve a fresh budget before reconnecting.
    reserved_tokens: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum ScheduleReservation {
    Ready,
    Waiting(String),
    Failed(Vec<SubagentId>),
}

struct CoordinatorState {
    revision: u64,
    next_id: u64,
    enabled: bool,
    capacity: u16,
    max_tokens_per_agent: u64,
    max_total_tokens_per_session: u64,
    availability_error: Option<String>,
    mcp_enabled: bool,
    mcp_status: UiNotice,
    profiles: AgentProfileCatalogSnapshot,
    records: BTreeMap<SubagentId, AgentRecord>,
}

impl CoordinatorState {
    fn session_token_budget(&self, session_id: &Option<String>) -> u64 {
        self.records
            .values()
            .filter(|record| &record.snapshot.session_id == session_id)
            .fold(self.max_total_tokens_per_session, |budget, record| {
                budget.saturating_add(
                    record
                        .snapshot
                        .token_budget
                        .saturating_sub(self.max_tokens_per_agent),
                )
            })
    }

    fn fleet_snapshot(&self) -> SubagentFleetSnapshot {
        let agents = self
            .records
            .values()
            .map(|record| record.snapshot.clone())
            .collect::<Vec<_>>();
        let mut session_budgets = BTreeMap::<Option<String>, u64>::new();
        for agent in &agents {
            session_budgets
                .entry(agent.session_id.clone())
                .or_insert_with(|| self.session_token_budget(&agent.session_id));
        }
        SubagentFleetSnapshot {
            revision: self.revision,
            enabled: self.enabled,
            capacity: self.capacity,
            active: agents
                .iter()
                .filter(|agent| agent.status.is_active())
                .count(),
            total_tokens: agents.iter().map(|agent| agent.total_tokens).sum(),
            token_budget: if session_budgets.is_empty() {
                self.max_total_tokens_per_session
            } else {
                session_budgets.values().copied().sum()
            },
            availability_error: self.availability_error.clone(),
            mcp_enabled: self.mcp_enabled,
            mcp_status: self.mcp_status.clone(),
            profiles: self.profiles.clone(),
            agents: Arc::from(agents),
        }
    }
}

#[derive(Clone)]
struct Reporter {
    id: SubagentId,
    state: Arc<Mutex<CoordinatorState>>,
    ui: watch::Sender<UiSnapshot>,
    persistence: Arc<Mutex<Option<mpsc::UnboundedSender<PersistenceUpdate>>>>,
    auto_approve_shell: Arc<AtomicBool>,
}

impl Reporter {
    fn reserve_request_budget(
        &self,
        estimated_input_tokens: u64,
        desired_output_tokens: u32,
    ) -> Result<RequestTokenReservation, SubagentError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        let record = state
            .records
            .get(&self.id)
            .ok_or(SubagentError::Unknown(self.id))?;
        let session_id = record.snapshot.session_id.clone();
        let agent_used = record
            .snapshot
            .total_tokens
            .saturating_add(record.reserved_tokens);
        let session_used = state
            .records
            .values()
            .filter(|candidate| candidate.snapshot.session_id == session_id)
            .fold(0_u64, |total, candidate| {
                total
                    .saturating_add(candidate.snapshot.total_tokens)
                    .saturating_add(candidate.reserved_tokens)
            });
        let agent_limit = record.snapshot.token_budget;
        let session_limit = state.session_token_budget(&session_id);
        let agent_remaining = agent_limit.saturating_sub(agent_used);
        let session_remaining = session_limit.saturating_sub(session_used);
        let remaining = agent_remaining.min(session_remaining);
        if estimated_input_tokens >= remaining {
            let (scope, used, limit) = if agent_remaining <= session_remaining {
                (SubagentBudgetScope::Agent, agent_used, agent_limit)
            } else {
                (
                    SubagentBudgetScope::SessionTree,
                    session_used,
                    session_limit,
                )
            };
            return Err(SubagentError::TokenBudgetExhausted { scope, used, limit });
        }
        let granted_output =
            u64::from(desired_output_tokens).min(remaining.saturating_sub(estimated_input_tokens));
        let granted_output = u32::try_from(granted_output).map_err(|_| {
            SubagentError::Protocol("reserved output token count does not fit u32".to_owned())
        })?;
        if granted_output == 0 {
            let (scope, used, limit) = if agent_remaining <= session_remaining {
                (SubagentBudgetScope::Agent, agent_used, agent_limit)
            } else {
                (
                    SubagentBudgetScope::SessionTree,
                    session_used,
                    session_limit,
                )
            };
            return Err(SubagentError::TokenBudgetExhausted { scope, used, limit });
        }
        let reserved = estimated_input_tokens.saturating_add(u64::from(granted_output));
        let record = state
            .records
            .get_mut(&self.id)
            .ok_or(SubagentError::Unknown(self.id))?;
        record.reserved_tokens = record.reserved_tokens.saturating_add(reserved);
        Ok(RequestTokenReservation {
            reporter: self.clone(),
            reserved,
            granted_output,
            active: true,
        })
    }

    fn finish_request_budget(
        &self,
        reserved: u64,
        input: u64,
        output: u64,
        total: u64,
    ) -> Result<(), SubagentError> {
        let (fleet, persisted) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&self.id)
                .ok_or(SubagentError::Unknown(self.id))?;
            record.reserved_tokens = record.reserved_tokens.saturating_sub(reserved);
            record.snapshot.input_tokens = record.snapshot.input_tokens.saturating_add(input);
            record.snapshot.output_tokens = record.snapshot.output_tokens.saturating_add(output);
            record.snapshot.total_tokens = record.snapshot.total_tokens.saturating_add(total);
            record.snapshot.revision = record.snapshot.revision.saturating_add(1);
            record.snapshot.updated_at = Utc::now();
            let persisted =
                PersistedAgent::from_snapshot(&record.snapshot, record.recovery.clone());
            state.revision = state.revision.saturating_add(1);
            (state.fleet_snapshot(), persisted)
        };
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        self.persist(persisted);
        Ok(())
    }

    fn release_request_budget(&self, reserved: u64) {
        if let Ok(mut state) = self.state.lock()
            && let Some(record) = state.records.get_mut(&self.id)
        {
            record.reserved_tokens = record.reserved_tokens.saturating_sub(reserved);
        }
    }

    fn mutate<F>(&self, mutation: F) -> Result<SubagentSnapshot, SubagentError>
    where
        F: FnOnce(&mut AgentRecord),
    {
        let (fleet, snapshot, persisted) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&self.id)
                .ok_or(SubagentError::Unknown(self.id))?;
            mutation(record);
            record.snapshot.revision = record.snapshot.revision.saturating_add(1);
            record.snapshot.updated_at = Utc::now();
            let snapshot = record.snapshot.clone();
            let persisted = PersistedAgent::from_snapshot(&snapshot, record.recovery.clone());
            state.revision = state.revision.saturating_add(1);
            let fleet = state.fleet_snapshot();
            (fleet, snapshot, persisted)
        };
        self.ui.send_modify(|snapshot| {
            snapshot.subagents = fleet;
        });
        self.persist(persisted);
        Ok(snapshot)
    }

    fn persist(&self, record: PersistedAgent) {
        let sender = self
            .persistence
            .lock()
            .ok()
            .and_then(|sender| sender.as_ref().cloned());
        if let Some(sender) = sender
            && sender
                .send(PersistenceUpdate {
                    record,
                    acknowledgement: None,
                })
                .is_err()
        {
            tracing::error!(agent_id = %self.id, "sub-agent journal writer is unavailable");
        }
    }

    async fn persist_durable(&self, record: PersistedAgent) -> Result<(), SubagentError> {
        let sender = self
            .persistence
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?
            .as_ref()
            .cloned()
            .ok_or(SubagentError::PersistenceUnavailable)?;
        let (acknowledgement, received) = oneshot::channel();
        sender
            .send(PersistenceUpdate {
                record,
                acknowledgement: Some(acknowledgement),
            })
            .map_err(|_| SubagentError::PersistenceUnavailable)?;
        received
            .await
            .map_err(|_| SubagentError::PersistenceAcknowledgement)?
            .map_err(|error| SubagentError::Unavailable(format!("journal sync failed: {error}")))
    }

    async fn persist_current_durable(&self) -> Result<(), SubagentError> {
        let record = {
            let state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get(&self.id)
                .ok_or(SubagentError::Unknown(self.id))?;
            PersistedAgent::from_snapshot(&record.snapshot, record.recovery.clone())
        };
        self.persist_durable(record).await
    }

    async fn checkpoint(&self, recovery: RecoveryState) -> Result<(), SubagentError> {
        validate_recovery_state(&recovery)?;
        let (fleet, persisted) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&self.id)
                .ok_or(SubagentError::Unknown(self.id))?;
            record.snapshot.recovery = Some(recovery.summary(record.worktree.is_some()));
            record.recovery = Some(recovery);
            record.snapshot.revision = record.snapshot.revision.saturating_add(1);
            record.snapshot.updated_at = Utc::now();
            let persisted =
                PersistedAgent::from_snapshot(&record.snapshot, record.recovery.clone());
            state.revision = state.revision.saturating_add(1);
            (state.fleet_snapshot(), persisted)
        };
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        self.persist_durable(persisted).await
    }

    fn mark_recovery(&self, reason: String, error: Option<String>) {
        let _ = self.mutate(|record| {
            record.snapshot.status = SubagentStatus::RecoveryRequired;
            record.snapshot.completed_at = None;
            record.snapshot.last_message.clone_from(&reason);
            record.snapshot.error = error;
            record.snapshot.pending_command = None;
            record.approval = None;
            if let Some(recovery) = &mut record.recovery {
                recovery.reason.clone_from(&reason);
                recovery.checkpoint_at = Utc::now();
                record.snapshot.recovery = Some(recovery.summary(record.worktree.is_some()));
            }
            push_transcript(
                &mut record.snapshot,
                "recovery required".to_owned(),
                &reason,
            );
        });
    }

    fn can_recover(&self) -> bool {
        self.state.lock().ok().is_some_and(|state| {
            state.records.get(&self.id).is_some_and(|record| {
                record.snapshot.mode == SubagentMode::Writer
                    && record
                        .recovery
                        .as_ref()
                        .is_some_and(|recovery| recovery.can_resume(record.worktree.is_some()))
            })
        })
    }

    fn try_reserve_schedule(&self) -> Result<ScheduleReservation, SubagentError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        let record = state
            .records
            .get(&self.id)
            .ok_or(SubagentError::Unknown(self.id))?;
        let dependencies = record.snapshot.dependencies.to_vec();
        let mode = record.snapshot.mode;
        let file_claims = record.snapshot.file_claims.to_vec();
        let dependency_states = dependencies
            .iter()
            .map(|dependency| {
                let dependency_record = state
                    .records
                    .get(dependency)
                    .ok_or(ScheduleError::UnknownDependency(dependency.get()))?;
                Ok((
                    dependency.get(),
                    dependency_state(&dependency_record.snapshot),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ScheduleError>>()?;
        match dependency_decision(
            &dependencies
                .iter()
                .map(|dependency| dependency.get())
                .collect::<Vec<_>>(),
            &dependency_states,
        )? {
            DependencyDecision::Waiting(ids) => {
                let ids = ids.into_iter().map(SubagentId::new).collect::<Vec<_>>();
                return Ok(ScheduleReservation::Waiting(format!(
                    "Waiting for dependencies: {}",
                    display_agent_ids(&ids)
                )));
            }
            DependencyDecision::Failed(ids) => {
                return Ok(ScheduleReservation::Failed(
                    ids.into_iter().map(SubagentId::new).collect(),
                ));
            }
            DependencyDecision::Ready => {}
        }

        if mode == SubagentMode::Writer {
            let conflicts = state
                .records
                .iter()
                .filter(|(id, candidate)| {
                    **id != self.id
                        && candidate.schedule_reserved
                        && candidate.snapshot.mode == SubagentMode::Writer
                        && !is_descendant_in_state(&state, **id, self.id)
                        && writer_claims_conflict(
                            &file_claims,
                            candidate.snapshot.file_claims.as_ref(),
                        )
                })
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            if !conflicts.is_empty() {
                return Ok(ScheduleReservation::Waiting(format!(
                    "Waiting for writer file claims held by {}",
                    display_agent_ids(&conflicts)
                )));
            }
        }
        let record = state
            .records
            .get_mut(&self.id)
            .ok_or(SubagentError::Unknown(self.id))?;
        record.schedule_reserved = mode == SubagentMode::Writer;
        Ok(ScheduleReservation::Ready)
    }

    fn waiting_for_schedule(&self, message: &str) -> Result<(), SubagentError> {
        let unchanged = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?
            .records
            .get(&self.id)
            .is_some_and(|record| {
                record.snapshot.status == SubagentStatus::WaitingDependencies
                    && record.snapshot.last_message == message
            });
        if unchanged {
            return Ok(());
        }
        self.mutate(|record| {
            record.snapshot.status = SubagentStatus::WaitingDependencies;
            record.snapshot.last_message = message.to_owned();
            push_transcript(&mut record.snapshot, "scheduler".to_owned(), message);
        })?;
        Ok(())
    }

    fn dependency_handoff(&self) -> Result<String, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        let record = state
            .records
            .get(&self.id)
            .ok_or(SubagentError::Unknown(self.id))?;
        let mut handoff =
            String::from("DEPENDENCY HANDOFFS (authoritative completed predecessors):\n");
        for dependency in record.snapshot.dependencies.iter() {
            let predecessor = state
                .records
                .get(dependency)
                .ok_or(ScheduleError::UnknownDependency(dependency.get()))?;
            handoff.push_str(&format!(
                "\n- {} [{}] {}\n  Result: {}\n  Reviewed files: {}\n",
                predecessor.snapshot.id,
                predecessor.snapshot.profile_name,
                predecessor.snapshot.label,
                if predecessor.snapshot.result.is_empty() {
                    predecessor.snapshot.last_message.as_str()
                } else {
                    predecessor.snapshot.result.as_str()
                },
                if predecessor.snapshot.resolved_files.is_empty() {
                    "none".to_owned()
                } else {
                    predecessor.snapshot.resolved_files.join(", ")
                }
            ));
            if handoff.len() >= MAX_DEPENDENCY_HANDOFF_BYTES {
                break;
            }
        }
        Ok(bounded_text(&handoff, MAX_DEPENDENCY_HANDOFF_BYTES))
    }

    fn status(&self, status: SubagentStatus, message: impl Into<String>) {
        let message = message.into();
        let _ = self.mutate(|record| {
            record.snapshot.status = status;
            record.snapshot.last_message.clone_from(&message);
            if status == SubagentStatus::Starting && record.snapshot.started_at.is_none() {
                record.snapshot.started_at = Some(Utc::now());
            }
            if status.is_terminal() {
                record.snapshot.completed_at = Some(Utc::now());
                record.snapshot.pending_command = None;
                record.snapshot.pending_budget = None;
                record.snapshot.recovery = None;
                record.approval = None;
                record.budget_approval = None;
                record.recovery = None;
                if status == SubagentStatus::Completed || record.snapshot.changed_files.is_empty() {
                    record.schedule_reserved = false;
                }
            }
            push_transcript(&mut record.snapshot, status.to_string(), &message);
        });
    }

    fn iteration(&self, message: impl Into<String>) {
        let message = message.into();
        let _ = self.mutate(|record| {
            record.snapshot.tool_iterations = record.snapshot.tool_iterations.saturating_add(1);
            record.snapshot.last_message.clone_from(&message);
            push_transcript(&mut record.snapshot, "tool loop".to_owned(), &message);
        });
    }

    fn assistant(&self, content: &str) {
        let content = bounded_text(content, MAX_TRANSCRIPT_ENTRY_BYTES);
        let _ = self.mutate(|record| {
            record.snapshot.last_message = compact_line(&content, 240);
            push_transcript(&mut record.snapshot, "assistant".to_owned(), &content);
        });
    }

    fn tool(&self, action: &ToolAction, outcome: &ToolOutcome) {
        let content = match outcome {
            ToolOutcome::Success(output) => format!("{}: {output}", action.tool_name()),
            ToolOutcome::Failure { message } => {
                format!("{} failed: {message}", action.tool_name())
            }
            ToolOutcome::Declined { .. } => format!("{} declined", action.tool_name()),
        };
        let content = bounded_text(&content, MAX_TRANSCRIPT_ENTRY_BYTES);
        let _ = self.mutate(|record| {
            record.snapshot.last_message = compact_line(&content, 240);
            push_transcript(&mut record.snapshot, "tool".to_owned(), &content);
        });
    }

    fn mcp_tool(&self, label: &str, outcome: &McpCallOutput) {
        let content = bounded_text(
            &format!(
                "{label} {}: {}",
                if outcome.is_error {
                    "failed"
                } else {
                    "completed"
                },
                outcome.content
            ),
            MAX_TRANSCRIPT_ENTRY_BYTES,
        );
        let _ = self.mutate(|record| {
            record.snapshot.last_message = compact_line(&content, 240);
            push_transcript(&mut record.snapshot, "MCP tool".to_owned(), &content);
        });
    }

    async fn set_worktree(&self, worktree: ManagedWorktree) -> Result<(), SubagentError> {
        self.mutate(|record| {
            record.snapshot.worktree = Some(worktree.path.display().to_string());
            record.snapshot.base_commit = Some(worktree.base_commit.clone());
            record.worktree = Some(worktree);
        })?;
        self.persist_current_durable().await
    }

    fn set_changes(&self, changes: WorktreeChangeSet) {
        let paths = changes
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        let digest = changes.digest_hex();
        let _ = self.mutate(|record| {
            record.snapshot.changed_files = Arc::from(paths);
            record.snapshot.change_digest = Some(digest);
            record.changes = Some(changes);
        });
    }

    async fn request_command_approval(
        &self,
        action_id: u64,
        command: String,
        model_requested_confirmation: bool,
        cancel: &CancellationToken,
    ) -> Result<bool, SubagentError> {
        let safe_for_session_policy = matches!(
            crate::tools::exec::confirmation_decision(&command, model_requested_confirmation),
            ConfirmationDecision::RequiresUserConfirmation {
                reason: ConfirmationReason::PolicyRequired
            }
        );
        if self.auto_approve_shell.load(Ordering::Acquire) && safe_for_session_policy {
            let _ = self.mutate(|record| {
                push_transcript(
                    &mut record.snapshot,
                    "auto approval".to_owned(),
                    "Shell command approved by the session Auto-Approval Center",
                );
            });
            return Ok(true);
        }
        let (tx, rx) = oneshot::channel();
        self.mutate(|record| {
            record.snapshot.status = SubagentStatus::WaitingApproval;
            record.snapshot.pending_command = Some(SubagentPendingCommand {
                action_id,
                command: command.clone(),
                model_requested_confirmation,
                mcp: false,
            });
            record.approval = Some(tx);
            push_transcript(
                &mut record.snapshot,
                "approval".to_owned(),
                "Shell command is waiting for an explicit user decision",
            );
        })?;
        let decision = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(SubagentError::Api(ApiError::Cancelled)),
            result = rx => {
                if cancel.is_cancelled() {
                    Err(SubagentError::Api(ApiError::Cancelled))
                } else {
                    Ok(result.unwrap_or(false))
                }
            },
        };
        self.mutate(|record| {
            record.snapshot.pending_command = None;
            if decision.is_ok() && record.snapshot.status == SubagentStatus::WaitingApproval {
                record.snapshot.status = SubagentStatus::Running;
            }
            record.approval = None;
        })?;
        decision
    }

    async fn request_mcp_approval(
        &self,
        action_id: u64,
        label: String,
        arguments: &Value,
        cancel: &CancellationToken,
    ) -> Result<bool, SubagentError> {
        let (tx, rx) = oneshot::channel();
        let arguments = bounded_text(&arguments.to_string(), 8 * 1024);
        self.mutate(|record| {
            record.snapshot.status = SubagentStatus::WaitingApproval;
            record.snapshot.pending_command = Some(SubagentPendingCommand {
                action_id,
                command: format!("MCP {label}\nArguments: {arguments}"),
                model_requested_confirmation: true,
                mcp: true,
            });
            record.approval = Some(tx);
            push_transcript(
                &mut record.snapshot,
                "MCP approval".to_owned(),
                &format!("{label} is waiting for an explicit user decision"),
            );
        })?;
        let decision = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(SubagentError::Api(ApiError::Cancelled)),
            result = rx => {
                if cancel.is_cancelled() {
                    Err(SubagentError::Api(ApiError::Cancelled))
                } else {
                    Ok(result.unwrap_or(false))
                }
            },
        };
        self.mutate(|record| {
            record.snapshot.pending_command = None;
            if decision.is_ok() && record.snapshot.status == SubagentStatus::WaitingApproval {
                record.snapshot.status = SubagentStatus::Running;
            }
            record.approval = None;
        })?;
        decision
    }

    async fn request_budget_increase(
        &self,
        scope: SubagentBudgetScope,
        used: u64,
        limit: u64,
        cancel: &CancellationToken,
    ) -> Result<bool, SubagentError> {
        let (tx, rx) = oneshot::channel();
        self.mutate(|record| {
            record.snapshot.status = SubagentStatus::WaitingBudget;
            record.snapshot.last_message = format!(
                "Token budget exhausted for {scope}: {used}/{limit}; waiting for user decision"
            );
            record.snapshot.pending_budget = Some(SubagentPendingBudget {
                scope,
                used,
                limit,
                suggested_increase: TOKEN_BUDGET_INCREMENT,
            });
            record.budget_approval = Some(tx);
            push_transcript(
                &mut record.snapshot,
                "token budget".to_owned(),
                &format!("{scope} budget reached {used}/{limit}; waiting for Raise budget or Stop"),
            );
        })?;
        let approved = tokio::select! {
            _ = cancel.cancelled() => false,
            result = rx => result.unwrap_or(false),
        };
        self.mutate(|record| {
            record.snapshot.pending_budget = None;
            record.budget_approval = None;
            if approved {
                record.snapshot.token_budget = record
                    .snapshot
                    .token_budget
                    .saturating_add(TOKEN_BUDGET_INCREMENT);
                record.snapshot.status = SubagentStatus::Running;
                record.snapshot.last_message =
                    format!("Token budget raised by {TOKEN_BUDGET_INCREMENT}; continuing");
                push_transcript(
                    &mut record.snapshot,
                    "token budget".to_owned(),
                    &format!("User raised the branch budget by {TOKEN_BUDGET_INCREMENT} tokens"),
                );
            } else {
                record.snapshot.last_message =
                    "User stopped the branch at its token budget".to_owned();
                push_transcript(
                    &mut record.snapshot,
                    "token budget".to_owned(),
                    "User chose Stop branch instead of increasing the token budget",
                );
            }
        })?;
        Ok(approved)
    }
}

struct RequestTokenReservation {
    reporter: Reporter,
    reserved: u64,
    granted_output: u32,
    active: bool,
}

impl RequestTokenReservation {
    const fn granted_output(&self) -> u32 {
        self.granted_output
    }

    fn commit(
        mut self,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    ) -> Result<(), SubagentError> {
        self.reporter.finish_request_budget(
            self.reserved,
            input_tokens,
            output_tokens,
            total_tokens,
        )?;
        self.active = false;
        Ok(())
    }
}

impl Drop for RequestTokenReservation {
    fn drop(&mut self) {
        if self.active {
            self.reporter.release_request_budget(self.reserved);
        }
    }
}

pub struct SubagentCoordinator {
    owner: bool,
    api: ApiConfig,
    agent: AgentConfig,
    client: Arc<ResponsesClient>,
    profiles: Arc<Mutex<AgentProfileCatalog>>,
    state: Arc<Mutex<CoordinatorState>>,
    manager: Arc<tokio::sync::RwLock<Option<WorktreeManager>>>,
    semaphore: Arc<Semaphore>,
    handles: Arc<Mutex<BTreeMap<SubagentId, JoinHandle<()>>>>,
    persistence: Arc<Mutex<Option<mpsc::UnboundedSender<PersistenceUpdate>>>>,
    persistence_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    ui: watch::Sender<UiSnapshot>,
    auto_approve_shell: Arc<AtomicBool>,
    mcp_config: Option<McpConfig>,
    mcp: Arc<tokio::sync::RwLock<Option<Arc<McpManager>>>>,
    allow_mcp: Arc<AtomicBool>,
}

impl std::fmt::Debug for SubagentCoordinator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentCoordinator")
            .field("config", &self.agent.subagents)
            .finish_non_exhaustive()
    }
}

impl Clone for SubagentCoordinator {
    fn clone(&self) -> Self {
        Self {
            owner: false,
            api: self.api.clone(),
            agent: self.agent.clone(),
            client: Arc::clone(&self.client),
            profiles: Arc::clone(&self.profiles),
            state: Arc::clone(&self.state),
            manager: Arc::clone(&self.manager),
            semaphore: Arc::clone(&self.semaphore),
            handles: Arc::clone(&self.handles),
            persistence: Arc::clone(&self.persistence),
            persistence_handle: Arc::clone(&self.persistence_handle),
            ui: self.ui.clone(),
            auto_approve_shell: Arc::clone(&self.auto_approve_shell),
            mcp_config: self.mcp_config.clone(),
            mcp: Arc::clone(&self.mcp),
            allow_mcp: Arc::clone(&self.allow_mcp),
        }
    }
}

impl SubagentCoordinator {
    pub fn new(
        api: ApiConfig,
        agent: AgentConfig,
        ui: watch::Sender<UiSnapshot>,
    ) -> Result<Self, SubagentError> {
        Self::new_with_mcp(api, agent, ui, None)
    }

    pub fn new_with_mcp(
        api: ApiConfig,
        agent: AgentConfig,
        ui: watch::Sender<UiSnapshot>,
        mcp_config: Option<McpConfig>,
    ) -> Result<Self, SubagentError> {
        let client = Arc::new(ResponsesClient::new(api.clone())?);
        let capacity = agent.subagents.max_parallel;
        let enabled = agent.subagents.enabled;
        let max_tokens_per_agent = agent.subagents.max_tokens_per_agent;
        let max_total_tokens_per_session = agent.subagents.max_total_tokens_per_session;
        let mcp_enabled =
            agent.subagents.allow_mcp && mcp_config.as_ref().is_some_and(|config| config.enabled);
        let profiles = AgentProfileCatalog::load(agent.workspace_root.clone());
        let profile_snapshot = profiles.snapshot();
        Ok(Self {
            owner: true,
            api,
            agent,
            client,
            profiles: Arc::new(Mutex::new(profiles)),
            state: Arc::new(Mutex::new(CoordinatorState {
                revision: 0,
                next_id: 1,
                enabled,
                capacity,
                max_tokens_per_agent,
                max_total_tokens_per_session,
                availability_error: (!enabled).then(|| "disabled by configuration".to_owned()),
                mcp_enabled: false,
                mcp_status: if mcp_enabled {
                    UiNotice::SubagentMcpStarting
                } else {
                    UiNotice::SubagentMcpDisabled
                },
                profiles: profile_snapshot,
                records: BTreeMap::new(),
            })),
            manager: Arc::new(tokio::sync::RwLock::new(None)),
            semaphore: Arc::new(Semaphore::new(usize::from(capacity))),
            handles: Arc::new(Mutex::new(BTreeMap::new())),
            persistence: Arc::new(Mutex::new(None)),
            persistence_handle: Arc::new(Mutex::new(None)),
            ui,
            auto_approve_shell: Arc::new(AtomicBool::new(false)),
            mcp_config,
            mcp: Arc::new(tokio::sync::RwLock::new(None)),
            allow_mcp: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn set_auto_approve_shell(&self, enabled: bool) {
        self.auto_approve_shell.store(enabled, Ordering::Release);
    }

    pub async fn start(&self) {
        if !self.agent.subagents.enabled {
            self.publish();
            return;
        }
        match WorktreeManager::open(
            &self.agent.workspace_root,
            &self.agent.subagents.worktree_dir,
            self.agent.subagents.git_timeout,
        )
        .await
        {
            Ok(manager) => {
                *self.manager.write().await = Some(manager.clone());
                match self.restore_and_start_journal(&manager).await {
                    Ok(()) => self.set_availability(None),
                    Err(error) => self.set_availability(Some(error.to_string())),
                }
            }
            Err(error) => self.set_availability(Some(error.to_string())),
        }
        if self.agent.subagents.allow_mcp {
            let _ = self.set_mcp_enabled(true).await;
        }
    }

    pub async fn set_mcp_enabled(&self, enabled: bool) -> Result<(), SubagentError> {
        if !enabled {
            self.allow_mcp.store(false, Ordering::Release);
            *self.mcp.write().await = None;
            self.set_mcp_status(false, UiNotice::SubagentMcpDisabled);
            return Ok(());
        }
        self.allow_mcp.store(false, Ordering::Release);
        *self.mcp.write().await = None;
        self.set_mcp_status(false, UiNotice::SubagentMcpStarting);
        let result = async {
            let config = self.mcp_config.clone().ok_or_else(|| {
                SubagentError::Unavailable("MCP runtime is not configured".to_owned())
            })?;
            if !config.enabled {
                return Err(SubagentError::Unavailable(
                    "global MCP runtime is disabled".to_owned(),
                ));
            }
            let mut manager = McpManager::new(config)
                .map_err(|error| SubagentError::Protocol(error.to_string()))?;
            manager.start().await?;
            Ok::<_, SubagentError>(manager)
        }
        .await;
        let manager = match result {
            Ok(manager) => manager,
            Err(error) => {
                self.set_mcp_status(false, UiNotice::external(error.to_string()));
                return Err(error);
            }
        };
        let tool_count = manager.tools().len();
        *self.mcp.write().await = Some(Arc::new(manager));
        self.allow_mcp.store(true, Ordering::Release);
        self.set_mcp_status(true, UiNotice::McpToolsReady { count: tool_count });
        Ok(())
    }

    fn set_mcp_status(&self, enabled: bool, status: UiNotice) {
        let fleet = self.state.lock().map(|mut state| {
            state.mcp_enabled = enabled;
            state.mcp_status = status;
            state.revision = state.revision.saturating_add(1);
            state.fleet_snapshot()
        });
        if let Ok(fleet) = fleet {
            self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> SubagentFleetSnapshot {
        self.state.lock().map_or_else(
            |_| SubagentFleetSnapshot::default(),
            |state| state.fleet_snapshot(),
        )
    }

    pub fn agent_snapshot(&self, id: SubagentId) -> Result<SubagentSnapshot, SubagentError> {
        self.state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?
            .records
            .get(&id)
            .map(|record| record.snapshot.clone())
            .ok_or(SubagentError::Unknown(id))
    }

    fn ensure_descendant(
        &self,
        caller: SubagentId,
        target: SubagentId,
    ) -> Result<SubagentSnapshot, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        if !is_descendant_in_state(&state, caller, target) {
            return Err(SubagentError::DescendantAccess { id: target });
        }
        state
            .records
            .get(&target)
            .map(|record| record.snapshot.clone())
            .ok_or(SubagentError::Unknown(target))
    }

    fn descendants(&self, caller: SubagentId) -> Result<Vec<SubagentSnapshot>, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        Ok(state
            .records
            .iter()
            .filter(|(id, _)| is_descendant_in_state(&state, caller, **id))
            .map(|(_, record)| record.snapshot.clone())
            .collect())
    }

    fn has_unresolved_descendants(&self, caller: SubagentId) -> Result<bool, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        Ok(state.records.iter().any(|(id, record)| {
            descendant_requires_resolution(&record.snapshot)
                && is_descendant_in_state(&state, caller, *id)
        }))
    }

    fn integration_workspace_for_agent(&self, id: SubagentId) -> Result<PathBuf, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        let record = state.records.get(&id).ok_or(SubagentError::Unknown(id))?;
        let mut parent_id = record.snapshot.parent_id;
        let mut remaining = state.records.len();
        while let Some(parent) = parent_id {
            if remaining == 0 {
                return Err(SubagentError::Protocol(
                    "sub-agent parent graph contains a cycle".to_owned(),
                ));
            }
            remaining = remaining.saturating_sub(1);
            let parent_record = state
                .records
                .get(&parent)
                .ok_or(SubagentError::Unknown(parent))?;
            if parent_record.snapshot.mode == SubagentMode::Writer {
                return parent_record
                    .worktree
                    .as_ref()
                    .map(|worktree| worktree.path.clone())
                    .ok_or_else(|| {
                        SubagentError::Unavailable(format!(
                            "parent writer {parent} has no recoverable worktree"
                        ))
                    });
            }
            parent_id = parent_record.snapshot.parent_id;
        }
        Ok(self.agent.workspace_root.clone())
    }

    async fn manager_for_workspace(
        &self,
        workspace: &Path,
    ) -> Result<WorktreeManager, SubagentError> {
        let root_manager = self.manager.read().await.clone().ok_or_else(|| {
            SubagentError::Unavailable("worktree manager did not start".to_owned())
        })?;
        if workspace == root_manager.workspace_root() {
            return Ok(root_manager);
        }
        WorktreeManager::open(
            workspace,
            &self.agent.subagents.worktree_dir,
            self.agent.subagents.git_timeout,
        )
        .await
        .map_err(Into::into)
    }

    async fn manager_for_agent(&self, id: SubagentId) -> Result<WorktreeManager, SubagentError> {
        let workspace = self.integration_workspace_for_agent(id)?;
        self.manager_for_workspace(&workspace).await
    }

    fn cancel_active_descendants(&self, caller: SubagentId) {
        let targets = self.state.lock().ok().map(|state| {
            state
                .records
                .iter()
                .filter(|(id, record)| {
                    record.snapshot.status.is_active()
                        && is_descendant_in_state(&state, caller, **id)
                })
                .map(|(_, record)| record.cancel.clone())
                .collect::<Vec<_>>()
        });
        if let Some(targets) = targets {
            for target in targets {
                target.cancel();
            }
        }
    }

    async fn wait_for_descendants_settled(
        &self,
        caller: SubagentId,
        cancel: &CancellationToken,
    ) -> Result<(), SubagentError> {
        let mut updates = self.ui.subscribe();
        loop {
            if !self.has_unresolved_descendants(caller)? {
                return Ok(());
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(SubagentError::Api(ApiError::Cancelled)),
                changed = updates.changed() => {
                    changed.map_err(|_| SubagentError::Unavailable(
                        "sub-agent status stream closed".to_owned()
                    ))?;
                }
            }
        }
    }

    async fn spawn_child(
        &self,
        parent: SubagentId,
        parent_workspace: PathBuf,
        request: SpawnSubagentRequest,
    ) -> Result<SubagentId, SubagentError> {
        let parent_snapshot = self.agent_snapshot(parent)?;
        self.spawn_at(
            request,
            Some(parent),
            parent_snapshot.depth,
            Some(parent_workspace),
        )
        .await
    }

    pub async fn reload_profiles(&self) -> Result<AgentProfileCatalogSnapshot, SubagentError> {
        let profiles = Arc::clone(&self.profiles);
        let profile_snapshot = tokio::task::spawn_blocking(move || {
            let mut profiles = profiles
                .lock()
                .map_err(|_| SubagentError::ProfileStatePoisoned)?;
            profiles.reload();
            Ok::<_, SubagentError>(profiles.snapshot())
        })
        .await
        .map_err(SubagentError::ProfileReloadTask)??;
        let fleet = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            state.profiles = profile_snapshot.clone();
            state.revision = state.revision.saturating_add(1);
            state.fleet_snapshot()
        };
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        Ok(profile_snapshot)
    }

    pub async fn spawn(&self, request: SpawnSubagentRequest) -> Result<SubagentId, SubagentError> {
        self.spawn_at(request, None, 0, None).await
    }

    async fn spawn_at(
        &self,
        request: SpawnSubagentRequest,
        parent_id: Option<SubagentId>,
        parent_depth: u8,
        workspace_root: Option<PathBuf>,
    ) -> Result<SubagentId, SubagentError> {
        validate_text(
            &request.task,
            MAX_TASK_BYTES,
            SubagentError::EmptyTask,
            |actual_bytes, limit_bytes| SubagentError::TaskTooLarge {
                actual_bytes,
                limit_bytes,
            },
        )?;
        if !self.agent.subagents.enabled {
            return Err(SubagentError::Disabled);
        }
        let depth = parent_depth.saturating_add(1);
        if depth > self.agent.subagents.max_depth {
            return Err(SubagentError::DepthLimit {
                limit: self.agent.subagents.max_depth,
            });
        }
        let profile = self
            .profiles
            .lock()
            .map_err(|_| SubagentError::ProfileStatePoisoned)?
            .resolve(&request.profile_id)?;
        let file_claims = normalize_file_claims(&request.file_claims)?;
        if profile.mode == SubagentMode::Research && !file_claims.is_empty() {
            return Err(ScheduleError::ReadOnlyFileClaims.into());
        }
        let requested_dependencies = request.dependencies;
        let mut request = ResolvedSpawnSubagentRequest {
            task: request.task,
            session_id: request.session_id,
            deployment: profile.deployment.clone().unwrap_or(request.deployment),
            reasoning_effort: profile.reasoning_effort.unwrap_or(request.reasoning_effort),
            instructions: request.instructions,
            profile,
            dependencies: Arc::from([]),
            file_claims: Arc::from(file_claims),
            parent_id,
            depth,
            workspace_root: workspace_root.unwrap_or_else(|| self.agent.workspace_root.clone()),
        };
        let queued_recovery = if request.profile.mode == SubagentMode::Writer {
            Some(initial_recovery_state(&request.task, true)?)
        } else {
            None
        };
        if let Some(error) = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?
            .availability_error
            .clone()
        {
            return Err(SubagentError::Unavailable(error));
        }

        let (id, message_rx, reporter, cancel, restart_recovery) = {
            let (message_tx, message_rx) = mpsc::channel(16);
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            if let Some(parent_id) = request.parent_id {
                let parent = state
                    .records
                    .get(&parent_id)
                    .ok_or(SubagentError::Unknown(parent_id))?;
                if parent.snapshot.session_id != request.session_id
                    || parent.snapshot.depth != parent_depth
                {
                    return Err(SubagentError::DescendantAccess { id: parent_id });
                }
                if parent.snapshot.status != SubagentStatus::Running {
                    return Err(SubagentError::NotRunning { id: parent_id });
                }
                let children = state
                    .records
                    .values()
                    .filter(|record| record.snapshot.parent_id == Some(parent_id))
                    .count();
                if children >= usize::from(self.agent.subagents.max_children_per_agent) {
                    return Err(SubagentError::ChildLimit {
                        parent: parent_id,
                        limit: self.agent.subagents.max_children_per_agent,
                    });
                }
            }
            let in_session = state
                .records
                .values()
                .filter(|record| record.snapshot.session_id == request.session_id)
                .count();
            if in_session >= usize::from(self.agent.subagents.max_per_session) {
                return Err(SubagentError::SessionLimit {
                    limit: self.agent.subagents.max_per_session,
                });
            }
            let id = SubagentId::new(state.next_id);
            let graph = state
                .records
                .iter()
                .map(|(id, record)| {
                    (
                        id.get(),
                        record
                            .snapshot
                            .dependencies
                            .iter()
                            .map(|dependency| dependency.get())
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let dependencies = normalize_dependencies(
                id.get(),
                &requested_dependencies
                    .iter()
                    .map(|dependency| dependency.get())
                    .collect::<Vec<_>>(),
                &graph,
            )?
            .into_iter()
            .map(SubagentId::new)
            .collect::<Vec<_>>();
            for dependency in &dependencies {
                let dependency_record = state
                    .records
                    .get(dependency)
                    .ok_or(SubagentError::Unknown(*dependency))?;
                if dependency_record.snapshot.session_id != request.session_id {
                    return Err(SubagentError::DependencyOwnership { id: *dependency });
                }
                if let Some(parent_id) = request.parent_id
                    && !is_descendant_in_state(&state, parent_id, *dependency)
                {
                    return Err(SubagentError::DescendantAccess { id: *dependency });
                }
            }
            request.dependencies = Arc::from(dependencies.clone());
            state.next_id = state.next_id.saturating_add(1);
            let now = Utc::now();
            let cancel = CancellationToken::new();
            let restart_recovery = Arc::new(AtomicBool::new(false));
            let snapshot = SubagentSnapshot {
                id,
                parent_id: request.parent_id,
                depth: request.depth,
                revision: 1,
                session_id: request.session_id.clone(),
                label: task_label(&request.task),
                task: bounded_text(request.task.trim(), MAX_TASK_BYTES),
                profile_id: request.profile.id.clone(),
                profile_name: request.profile.name.clone(),
                mode: request.profile.mode,
                status: if dependencies.is_empty() {
                    SubagentStatus::Queued
                } else {
                    SubagentStatus::WaitingDependencies
                },
                deployment: request.deployment.clone(),
                reasoning_effort: request.reasoning_effort,
                created_at: now,
                started_at: None,
                completed_at: None,
                updated_at: now,
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                token_budget: self.agent.subagents.max_tokens_per_agent,
                tool_iterations: 0,
                last_message: if dependencies.is_empty() {
                    "Waiting for an execution slot".to_owned()
                } else {
                    format!(
                        "Waiting for dependencies: {}",
                        display_agent_ids(&dependencies)
                    )
                },
                result: String::new(),
                error: None,
                worktree: None,
                base_commit: None,
                changed_files: Arc::from([]),
                resolved_files: Arc::from([]),
                change_digest: None,
                pending_command: None,
                pending_budget: None,
                transcript: Arc::from([]),
                recovery: None,
                dependencies: Arc::from(dependencies),
                file_claims: Arc::clone(&request.file_claims),
            };
            state.records.insert(
                id,
                AgentRecord {
                    snapshot,
                    cancel: cancel.clone(),
                    messages: message_tx,
                    approval: None,
                    budget_approval: None,
                    restart_recovery: Arc::clone(&restart_recovery),
                    worktree: None,
                    changes: None,
                    recovery: queued_recovery,
                    schedule_reserved: false,
                    reserved_tokens: 0,
                },
            );
            state.revision = state.revision.saturating_add(1);
            let fleet = state.fleet_snapshot();
            let reporter = Reporter {
                id,
                state: Arc::clone(&self.state),
                ui: self.ui.clone(),
                persistence: Arc::clone(&self.persistence),
                auto_approve_shell: Arc::clone(&self.auto_approve_shell),
            };
            (id, message_rx, reporter, cancel, restart_recovery, fleet)
        }
        .pipe(|(id, rx, reporter, cancel, restart_recovery, fleet)| {
            self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
            (id, rx, reporter, cancel, restart_recovery)
        });

        if let Err(error) = reporter.persist_current_durable().await {
            let fleet = self.state.lock().ok().map(|mut state| {
                state.records.remove(&id);
                state.revision = state.revision.saturating_add(1);
                state.fleet_snapshot()
            });
            if let Some(fleet) = fleet {
                self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
            }
            return Err(error);
        }

        let worker = Worker {
            coordinator: self.clone(),
            request,
            api: self.api.clone(),
            agent: self.agent.clone(),
            client: Arc::clone(&self.client),
            mcp: Arc::clone(&self.mcp),
            allow_mcp: Arc::clone(&self.allow_mcp),
            semaphore: Arc::clone(&self.semaphore),
            reporter,
            cancel,
            messages: message_rx,
            existing_worktree: None,
            recovery: None,
            restart_recovery,
            permit: None,
        };
        self.launch_worker(id, worker)?;
        Ok(id)
    }

    fn launch_worker(&self, id: SubagentId, worker: Worker) -> Result<(), SubagentError> {
        let handle = tokio::spawn(run_worker_task(worker, self.agent.subagents.task_timeout));
        self.handles
            .lock()
            .map_err(|_| SubagentError::WorkerRegistryPoisoned)?
            .insert(id, handle);
        Ok(())
    }

    pub async fn resume(
        &self,
        id: SubagentId,
        expected_revision: u64,
        instructions: String,
    ) -> Result<(), SubagentError> {
        let snapshot = self.agent_snapshot(id)?;
        if snapshot.revision != expected_revision {
            return Err(SubagentError::Stale { id });
        }
        if snapshot.mode != SubagentMode::Writer || !snapshot.status.is_recoverable() {
            return Err(SubagentError::NotRecoverable { id });
        }
        let profile = self
            .profiles
            .lock()
            .map_err(|_| SubagentError::ProfileStatePoisoned)?
            .resolve(&snapshot.profile_id)?;
        if profile.mode != SubagentMode::Writer {
            return Err(SubagentError::NotRecoverable { id });
        }
        let (message_tx, message_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let restart_recovery = Arc::new(AtomicBool::new(false));
        let (recovery, worktree, fleet, reporter) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&id)
                .ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision
                || !record.snapshot.status.is_recoverable()
            {
                return Err(SubagentError::Stale { id });
            }
            let worktree = record.worktree.clone();
            let recovery = record
                .recovery
                .as_mut()
                .ok_or(SubagentError::NotRecoverable { id })?;
            if !recovery.can_resume(worktree.is_some()) {
                return Err(SubagentError::NotRecoverable { id });
            }
            recovery.attempt = recovery.attempt.saturating_add(1);
            recovery.reason = format!("Recovery attempt {} queued", recovery.attempt);
            recovery.checkpoint_at = Utc::now();
            let recovery = recovery.clone();
            record.snapshot.status = SubagentStatus::Queued;
            record.snapshot.completed_at = None;
            record.snapshot.last_message = recovery.reason.clone();
            record.snapshot.result.clear();
            record.snapshot.error = None;
            record.snapshot.pending_command = None;
            record.snapshot.recovery = Some(recovery.summary(worktree.is_some()));
            record.snapshot.revision = record.snapshot.revision.saturating_add(1);
            record.snapshot.updated_at = Utc::now();
            record.cancel = cancel.clone();
            record.messages = message_tx;
            record.approval = None;
            record.restart_recovery = Arc::clone(&restart_recovery);
            push_transcript(
                &mut record.snapshot,
                "recovery queued".to_owned(),
                &recovery.reason,
            );
            state.revision = state.revision.saturating_add(1);
            let fleet = state.fleet_snapshot();
            let reporter = Reporter {
                id,
                state: Arc::clone(&self.state),
                ui: self.ui.clone(),
                persistence: Arc::clone(&self.persistence),
                auto_approve_shell: Arc::clone(&self.auto_approve_shell),
            };
            (recovery, worktree, fleet, reporter)
        };
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        if let Err(error) = reporter.persist_current_durable().await {
            reporter.mark_recovery(
                format!("Recovery could not start because its journal update failed: {error}"),
                Some(error.to_string()),
            );
            return Err(error);
        }

        let request = ResolvedSpawnSubagentRequest {
            task: snapshot.task,
            profile,
            session_id: snapshot.session_id,
            deployment: snapshot.deployment,
            reasoning_effort: snapshot.reasoning_effort,
            instructions,
            dependencies: snapshot.dependencies,
            file_claims: snapshot.file_claims,
            parent_id: snapshot.parent_id,
            depth: snapshot.depth,
            workspace_root: snapshot
                .worktree
                .as_deref()
                .map(PathBuf::from)
                .unwrap_or_else(|| self.agent.workspace_root.clone()),
        };
        self.launch_worker(
            id,
            Worker {
                coordinator: self.clone(),
                request,
                api: self.api.clone(),
                agent: self.agent.clone(),
                client: Arc::clone(&self.client),
                mcp: Arc::clone(&self.mcp),
                allow_mcp: Arc::clone(&self.allow_mcp),
                semaphore: Arc::clone(&self.semaphore),
                reporter,
                cancel,
                messages: message_rx,
                existing_worktree: worktree,
                recovery: Some(recovery),
                restart_recovery,
                permit: None,
            },
        )
    }

    pub async fn abandon_recovery(
        &self,
        id: SubagentId,
        expected_revision: u64,
    ) -> Result<(), SubagentError> {
        let reporter = Reporter {
            id,
            state: Arc::clone(&self.state),
            ui: self.ui.clone(),
            persistence: Arc::clone(&self.persistence),
            auto_approve_shell: Arc::clone(&self.auto_approve_shell),
        };
        let (persisted, fleet, abandoned_revision) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&id)
                .ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision {
                return Err(SubagentError::Stale { id });
            }
            if !record.snapshot.status.is_recoverable() {
                return Err(SubagentError::NotRecoverable { id });
            }
            let message = "Recovery abandoned; isolated file changes remain available for review";
            record.snapshot.status = SubagentStatus::Cancelled;
            record.snapshot.completed_at = Some(Utc::now());
            record.snapshot.last_message = message.to_owned();
            record.snapshot.error = None;
            record.snapshot.pending_command = None;
            record.snapshot.recovery = None;
            record.approval = None;
            record.schedule_reserved = !record.snapshot.changed_files.is_empty();
            push_transcript(
                &mut record.snapshot,
                "recovery abandoned".to_owned(),
                message,
            );
            record.snapshot.revision = record.snapshot.revision.saturating_add(1);
            record.snapshot.updated_at = Utc::now();
            let abandoned_revision = record.snapshot.revision;
            let persisted = PersistedAgent::from_snapshot(&record.snapshot, None);
            state.revision = state.revision.saturating_add(1);
            (persisted, state.fleet_snapshot(), abandoned_revision)
        };
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        if let Err(error) = reporter.persist_durable(persisted).await {
            reporter.mark_recovery(
                format!("Could not durably abandon recovery: {error}"),
                Some(error.to_string()),
            );
            return Err(error);
        }
        if let Ok(mut state) = self.state.lock()
            && let Some(record) = state.records.get_mut(&id)
            && record.snapshot.revision == abandoned_revision
        {
            record.recovery = None;
        }
        Ok(())
    }

    pub fn send_message(
        &self,
        id: SubagentId,
        expected_revision: u64,
        message: String,
    ) -> Result<(), SubagentError> {
        validate_text(
            &message,
            MAX_MESSAGE_BYTES,
            SubagentError::EmptyMessage,
            |actual_bytes, limit_bytes| SubagentError::MessageTooLarge {
                actual_bytes,
                limit_bytes,
            },
        )?;
        let sender = {
            let state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state.records.get(&id).ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision {
                return Err(SubagentError::Stale { id });
            }
            if !record.snapshot.status.is_active() {
                return Err(SubagentError::NotRunning { id });
            }
            record.messages.clone()
        };
        sender
            .try_send(message)
            .map_err(|_| SubagentError::NotRunning { id })
    }

    pub fn cancel(&self, id: SubagentId, expected_revision: u64) -> Result<(), SubagentError> {
        let targets = {
            let state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state.records.get(&id).ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision {
                return Err(SubagentError::Stale { id });
            }
            if !record.snapshot.status.is_active() {
                return Err(SubagentError::NotRunning { id });
            }
            state
                .records
                .iter()
                .filter(|(candidate, record)| {
                    (**candidate == id || is_descendant_in_state(&state, id, **candidate))
                        && record.snapshot.status.is_active()
                })
                .map(|(candidate, record)| (*candidate, record.cancel.clone()))
                .collect::<Vec<_>>()
        };
        for (target, token) in targets {
            let reporter = Reporter {
                id: target,
                state: Arc::clone(&self.state),
                ui: self.ui.clone(),
                persistence: Arc::clone(&self.persistence),
                auto_approve_shell: Arc::clone(&self.auto_approve_shell),
            };
            reporter.status(
                SubagentStatus::Cancelling,
                if target == id {
                    "Cancellation requested"
                } else {
                    "Ancestor cancellation cascaded to this child"
                },
            );
            token.cancel();
        }
        Ok(())
    }

    pub fn decide_command(
        &self,
        id: SubagentId,
        expected_revision: u64,
        action_id: u64,
        approved: bool,
    ) -> Result<(), SubagentError> {
        let sender = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&id)
                .ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision
                || record
                    .snapshot
                    .pending_command
                    .as_ref()
                    .is_none_or(|pending| pending.action_id != action_id)
            {
                return Err(SubagentError::Stale { id });
            }
            record
                .approval
                .take()
                .ok_or(SubagentError::NoPendingApproval { id })?
        };
        sender
            .send(approved)
            .map_err(|_| SubagentError::NotRunning { id })
    }

    pub fn decide_budget(
        &self,
        id: SubagentId,
        expected_revision: u64,
        approved: bool,
    ) -> Result<(), SubagentError> {
        let sender = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get_mut(&id)
                .ok_or(SubagentError::Unknown(id))?;
            if record.snapshot.revision != expected_revision
                || record.snapshot.pending_budget.is_none()
                || record.snapshot.status != SubagentStatus::WaitingBudget
            {
                return Err(SubagentError::Stale { id });
            }
            record
                .budget_approval
                .take()
                .ok_or(SubagentError::NoPendingBudgetDecision { id })?
        };
        sender
            .send(approved)
            .map_err(|_| SubagentError::NotRunning { id })
    }

    pub async fn wait_for_update(
        &self,
        id: SubagentId,
        expected_revision: u64,
        wait: Duration,
        cancel: &CancellationToken,
    ) -> Result<SubagentSnapshot, SubagentError> {
        let mut snapshots = self.ui.subscribe();
        let initial = observed_agent(&snapshots, id)?;
        if initial.revision != expected_revision
            || initial.status.is_terminal()
            || initial.status.is_recoverable()
        {
            return Ok(initial);
        }

        let wait_for_change = async {
            loop {
                snapshots.changed().await.map_err(|_| {
                    SubagentError::Unavailable("sub-agent status stream closed".to_owned())
                })?;
                let current = observed_agent(&snapshots, id)?;
                if current.revision != expected_revision
                    || current.status.is_terminal()
                    || current.status.is_recoverable()
                {
                    return Ok(current);
                }
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => Err(SubagentError::Api(ApiError::Cancelled)),
            result = timeout(wait, wait_for_change) => match result {
                Ok(result) => result,
                Err(_) => observed_agent(&snapshots, id),
            },
        }
    }

    pub fn file_review(
        &self,
        id: SubagentId,
        expected_revision: u64,
        change_digest: &str,
        path: &str,
    ) -> Result<SubagentFileReview, SubagentError> {
        let state = self
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?;
        let record = state.records.get(&id).ok_or(SubagentError::Unknown(id))?;
        if record.snapshot.revision != expected_revision {
            return Err(SubagentError::Stale { id });
        }
        if record.snapshot.change_digest.as_deref() != Some(change_digest) {
            return Err(SubagentError::StaleChanges {
                id,
                digest: change_digest.to_owned(),
            });
        }
        let change = record
            .changes
            .as_ref()
            .ok_or(SubagentError::NoPendingChanges { id })?
            .changes
            .iter()
            .find(|change| change.path == path)
            .cloned()
            .ok_or_else(|| SubagentError::UnknownChange {
                id,
                path: path.to_owned(),
            })?;
        let binary = change.is_binary();
        let review = (!binary)
            .then(|| change.review().map(Arc::new))
            .transpose()?;
        Ok(SubagentFileReview {
            agent_id: id,
            agent_revision: expected_revision,
            change_digest: change_digest.to_owned(),
            path: path.to_owned(),
            binary,
            review,
        })
    }

    pub async fn decide_file(
        &self,
        review: &SubagentFileReview,
        decision: SubagentFileDecision,
        cancel: CancellationToken,
    ) -> Result<(), SubagentError> {
        let change = {
            let state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            let record = state
                .records
                .get(&review.agent_id)
                .ok_or(SubagentError::Unknown(review.agent_id))?;
            if record.snapshot.revision != review.agent_revision {
                return Err(SubagentError::Stale {
                    id: review.agent_id,
                });
            }
            if record.snapshot.change_digest.as_deref() != Some(review.change_digest.as_str()) {
                return Err(SubagentError::StaleChanges {
                    id: review.agent_id,
                    digest: review.change_digest.clone(),
                });
            }
            record
                .changes
                .as_ref()
                .ok_or(SubagentError::NoPendingChanges {
                    id: review.agent_id,
                })?
                .changes
                .iter()
                .find(|change| change.path == review.path)
                .cloned()
                .ok_or_else(|| SubagentError::UnknownChange {
                    id: review.agent_id,
                    path: review.path.clone(),
                })?
        };
        if self.has_unresolved_descendants(review.agent_id)? {
            return Err(SubagentError::DescendantsPending {
                id: review.agent_id,
            });
        }
        let manager = self.manager_for_agent(review.agent_id).await?;
        match decision {
            SubagentFileDecision::TextHunks(decisions) if !change.is_binary() => {
                manager
                    .apply_text_decisions(change, decisions, cancel)
                    .await?;
            }
            SubagentFileDecision::ApproveBinary if change.is_binary() => {
                manager.apply_binary_whole(change, cancel).await?;
            }
            SubagentFileDecision::Reject => {}
            SubagentFileDecision::TextHunks(_) => {
                return Err(WorktreeError::BinaryReview(review.path.clone()).into());
            }
            SubagentFileDecision::ApproveBinary => {
                return Err(SubagentError::Protocol(
                    "whole-file binary approval was used for a text change".to_owned(),
                ));
            }
        }

        let reporter = Reporter {
            id: review.agent_id,
            state: Arc::clone(&self.state),
            ui: self.ui.clone(),
            persistence: Arc::clone(&self.persistence),
            auto_approve_shell: Arc::clone(&self.auto_approve_shell),
        };
        let updated = reporter.mutate(|record| {
            let mut resolved = record.snapshot.resolved_files.to_vec();
            if !resolved.iter().any(|path| path == &review.path) {
                resolved.push(review.path.clone());
            }
            record.snapshot.resolved_files = Arc::from(resolved);
            if let Some(changes) = &mut record.changes {
                changes.changes = Arc::from(
                    changes
                        .changes
                        .iter()
                        .filter(|change| change.path != review.path)
                        .cloned()
                        .collect::<Vec<_>>(),
                );
                record.snapshot.changed_files = Arc::from(
                    changes
                        .changes
                        .iter()
                        .map(|change| change.path.clone())
                        .collect::<Vec<_>>(),
                );
            }
            if record.snapshot.changed_files.is_empty() {
                record.snapshot.status = SubagentStatus::Completed;
                record.snapshot.completed_at = Some(Utc::now());
                record.snapshot.last_message = "All isolated changes were reviewed".to_owned();
                record.schedule_reserved = false;
            }
        })?;
        if updated.changed_files.is_empty() {
            let worktree = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?
                .records
                .get(&review.agent_id)
                .and_then(|record| record.worktree.clone());
            if let Some(worktree) = worktree {
                match manager.discard(&worktree).await {
                    Ok(()) => {
                        let _ = reporter.mutate(|record| {
                            record.worktree = None;
                            record.changes = None;
                            record.snapshot.worktree = None;
                            record.snapshot.base_commit = None;
                            record.snapshot.change_digest = None;
                        });
                    }
                    Err(error) => {
                        let message = format!("Changes were reviewed, but cleanup failed: {error}");
                        let _ = reporter.mutate(|record| {
                            record.snapshot.error = Some(message.clone());
                        });
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn shutdown(&self) {
        let tokens = self
            .state
            .lock()
            .map(|state| {
                state
                    .records
                    .values()
                    .filter(|record| record.snapshot.status.is_active())
                    .map(|record| {
                        record.restart_recovery.store(true, Ordering::Release);
                        record.cancel.clone()
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for token in tokens {
            token.cancel();
        }
        let handles = self
            .handles
            .lock()
            .map(|mut handles| std::mem::take(&mut *handles))
            .unwrap_or_default();
        let abort_handles = handles
            .values()
            .map(tokio::task::JoinHandle::abort_handle)
            .collect::<Vec<_>>();
        let wait_all = async {
            for (_, handle) in handles {
                let _ = handle.await;
            }
        };
        if timeout(SHUTDOWN_GRACE, wait_all).await.is_err() {
            for handle in abort_handles {
                handle.abort();
            }
        }
        if let Ok(mut sender) = self.persistence.lock() {
            *sender = None;
        }
        let persistence_handle = self
            .persistence_handle
            .lock()
            .ok()
            .and_then(|mut handle| handle.take());
        if let Some(handle) = persistence_handle {
            let _ = timeout(SHUTDOWN_GRACE, handle).await;
        }
    }

    async fn restore_and_start_journal(
        &self,
        manager: &WorktreeManager,
    ) -> Result<(), SubagentError> {
        let journal_path = manager.control_root().join("subagents.jsonl");
        let mut persisted = load_journal(&journal_path).await?;
        let dependency_graph = persisted
            .iter()
            .map(|(id, value)| {
                (
                    id.get(),
                    value
                        .dependencies
                        .iter()
                        .map(|dependency| dependency.get())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        validate_dependency_graph(&dependency_graph)?;
        validate_persisted_parent_tree(
            &persisted,
            self.agent.subagents.max_depth,
            self.agent.subagents.max_children_per_agent,
        )?;
        for value in persisted.values() {
            for dependency in &value.dependencies {
                let predecessor = persisted
                    .get(dependency)
                    .ok_or(ScheduleError::UnknownDependency(dependency.get()))?;
                if predecessor.session_id != value.session_id {
                    return Err(SubagentError::DependencyOwnership { id: *dependency });
                }
            }
        }
        for value in persisted.values_mut() {
            value.dependencies.sort_unstable();
            value.dependencies.dedup();
            value.file_claims = normalize_file_claims(&value.file_claims)?;
            if value.mode == SubagentMode::Research && !value.file_claims.is_empty() {
                return Err(ScheduleError::ReadOnlyFileClaims.into());
            }
        }
        let mut restored = Vec::with_capacity(persisted.len());
        let mut restored_worktrees = BTreeMap::<SubagentId, ManagedWorktree>::new();
        for value in persisted.values().cloned() {
            let mut recovery = value.recovery.clone();
            let mut snapshot = value.into_snapshot();
            if snapshot.token_budget == 0 {
                snapshot.token_budget = self.agent.subagents.max_tokens_per_agent;
            }
            let recover_writer = should_restore_writer(snapshot.mode, snapshot.status);
            if recover_writer {
                if recovery.is_none() {
                    let allow_fresh_worktree =
                        snapshot.status == SubagentStatus::Queued && snapshot.base_commit.is_none();
                    recovery = Some(initial_recovery_state(
                        &snapshot.task,
                        allow_fresh_worktree,
                    )?);
                }
                let reason = if snapshot.status.is_recoverable() {
                    recovery.as_ref().map_or_else(
                        || "Writer recovery is pending".to_owned(),
                        |state| state.reason.clone(),
                    )
                } else {
                    "Writer stopped before a durable terminal result; inspect the recovered worktree before resuming"
                        .to_owned()
                };
                if let Some(recovery) = &mut recovery {
                    recovery.reason.clone_from(&reason);
                    recovery.checkpoint_at = Utc::now();
                }
                snapshot.status = SubagentStatus::RecoveryRequired;
                snapshot.revision = snapshot.revision.saturating_add(1);
                snapshot.completed_at = None;
                snapshot.updated_at = Utc::now();
                snapshot.last_message = reason;
            } else if snapshot.status.is_active() {
                snapshot.status = SubagentStatus::Interrupted;
                snapshot.revision = snapshot.revision.saturating_add(1);
                snapshot.completed_at = Some(Utc::now());
                snapshot.updated_at = Utc::now();
                snapshot.last_message = "Interrupted by application restart".to_owned();
            }

            let mut worktree = None;
            let mut changes = None;
            if snapshot.mode == SubagentMode::Writer
                && let Some(base_commit) = snapshot.base_commit.clone()
            {
                let integration_workspace = persisted_integration_workspace(
                    &snapshot,
                    &persisted,
                    &restored_worktrees,
                    &self.agent.workspace_root,
                );
                let recovery_manager = match integration_workspace {
                    Ok(workspace) if workspace == manager.workspace_root() => Some(manager.clone()),
                    Ok(workspace) => match WorktreeManager::open(
                        &workspace,
                        &self.agent.subagents.worktree_dir,
                        self.agent.subagents.git_timeout,
                    )
                    .await
                    {
                        Ok(manager) => Some(manager),
                        Err(error) => {
                            snapshot.error = Some(format!(
                                "Nested writer integration workspace could not be opened: {error}"
                            ));
                            None
                        }
                    },
                    Err(error) => {
                        snapshot.error = Some(error.to_string());
                        None
                    }
                };
                let recovered = if let Some(recovery_manager) = recovery_manager {
                    Some((
                        recovery_manager
                            .recover(snapshot.id.get(), base_commit)
                            .await,
                        recovery_manager,
                    ))
                } else {
                    None
                };
                match recovered {
                    Some((Ok(recovered), recovery_manager)) => {
                        snapshot.worktree = Some(recovered.path.display().to_string());
                        match recovery_manager.collect_changes(&recovered).await {
                            Ok(collected) => {
                                let pending = collected
                                    .changes
                                    .iter()
                                    .filter(|change| {
                                        !snapshot
                                            .resolved_files
                                            .iter()
                                            .any(|path| path == &change.path)
                                    })
                                    .cloned()
                                    .collect::<Vec<_>>();
                                snapshot.changed_files = Arc::from(
                                    pending
                                        .iter()
                                        .map(|change| change.path.clone())
                                        .collect::<Vec<_>>(),
                                );
                                snapshot.change_digest = Some(collected.digest_hex());
                                changes = Some(WorktreeChangeSet {
                                    changes: Arc::from(pending),
                                    digest: collected.digest,
                                });
                            }
                            Err(error) => {
                                snapshot.error = Some(format!(
                                    "Could not inspect restored worktree changes: {error}"
                                ));
                            }
                        }
                        restored_worktrees.insert(snapshot.id, recovered.clone());
                        worktree = Some(recovered);
                    }
                    Some((Err(error), _)) => {
                        snapshot.error = Some(format!(
                            "Recorded worktree could not be recovered safely: {error}"
                        ));
                        snapshot.worktree = None;
                    }
                    None => {}
                }
            }
            snapshot.recovery = recovery
                .as_ref()
                .map(|state| state.summary(worktree.is_some()));
            let schedule_reserved = snapshot.mode == SubagentMode::Writer
                && worktree.is_some()
                && (snapshot.status.is_recoverable() || !snapshot.changed_files.is_empty());
            let (messages, receiver) = mpsc::channel(1);
            drop(receiver);
            restored.push((
                snapshot.id,
                AgentRecord {
                    snapshot,
                    cancel: CancellationToken::new(),
                    messages,
                    approval: None,
                    budget_approval: None,
                    restart_recovery: Arc::new(AtomicBool::new(false)),
                    worktree,
                    changes,
                    recovery,
                    schedule_reserved,
                    reserved_tokens: 0,
                },
            ));
        }

        let (persistence_tx, persistence_rx) = mpsc::unbounded_channel();
        {
            let mut sender = self
                .persistence
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            *sender = Some(persistence_tx.clone());
        }
        let writer_path = journal_path;
        let writer_latest = persisted;
        let handle = tokio::spawn(async move {
            if let Err(error) = journal_writer(writer_path, writer_latest, persistence_rx).await {
                tracing::error!(error = %error, "sub-agent journal writer stopped");
            }
        });
        {
            let mut persistence_handle = self
                .persistence_handle
                .lock()
                .map_err(|_| SubagentError::WorkerRegistryPoisoned)?;
            *persistence_handle = Some(handle);
        }

        let snapshots = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SubagentError::StatePoisoned)?;
            for (id, record) in restored {
                state.next_id = state.next_id.max(id.get().saturating_add(1));
                state.records.insert(id, record);
            }
            state.revision = state.revision.saturating_add(1);
            state
                .records
                .values()
                .map(|record| record.snapshot.clone())
                .collect::<Vec<_>>()
        };
        for snapshot in snapshots {
            let recovery = self.state.lock().ok().and_then(|state| {
                state
                    .records
                    .get(&snapshot.id)
                    .and_then(|record| record.recovery.clone())
            });
            let _ = persistence_tx.send(PersistenceUpdate {
                record: PersistedAgent::from_snapshot(&snapshot, recovery),
                acknowledgement: None,
            });
        }
        self.publish();
        Ok(())
    }

    fn set_availability(&self, error: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.availability_error = error;
            state.revision = state.revision.saturating_add(1);
            let fleet = state.fleet_snapshot();
            drop(state);
            self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
        }
    }

    fn publish(&self) {
        let fleet = self.snapshot();
        self.ui.send_modify(|snapshot| snapshot.subagents = fleet);
    }
}

async fn load_journal(path: &Path) -> Result<BTreeMap<SubagentId, PersistedAgent>, SubagentError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(SubagentError::PersistenceIo {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut latest = BTreeMap::new();
    for (line_index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<PersistedAgent>(line) {
            Ok(record) => {
                if let Some(recovery) = &record.recovery {
                    validate_recovery_state(recovery)?;
                }
                if latest
                    .get(&record.id)
                    .is_none_or(|previous: &PersistedAgent| record.revision >= previous.revision)
                {
                    latest.insert(record.id, record);
                }
            }
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    line = line_index.saturating_add(1),
                    error = %error,
                    "ignored malformed or interrupted sub-agent journal record"
                );
            }
        }
    }
    Ok(latest)
}

async fn journal_writer(
    path: PathBuf,
    mut latest: BTreeMap<SubagentId, PersistedAgent>,
    mut updates: mpsc::UnboundedReceiver<PersistenceUpdate>,
) -> Result<(), SubagentError> {
    while let Some(update) = updates.recv().await {
        let PersistenceUpdate {
            record,
            acknowledgement,
        } = update;
        if latest
            .get(&record.id)
            .is_some_and(|previous| previous.revision > record.revision)
        {
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement.send(Ok(()));
            }
            continue;
        }
        if let Err(error) = append_journal_record(&path, &record).await {
            if let Some(acknowledgement) = acknowledgement {
                let _ = acknowledgement.send(Err(PersistenceAckError {
                    message: error.to_string(),
                }));
            }
            return Err(error);
        }
        latest.insert(record.id, record);
        if let Some(acknowledgement) = acknowledgement {
            let _ = acknowledgement.send(Ok(()));
        }
        let length = tokio::fs::metadata(&path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if length >= JOURNAL_COMPACT_BYTES {
            let compact_path = path.clone();
            let compact_records = latest.clone();
            tokio::task::spawn_blocking(move || compact_journal(&compact_path, &compact_records))
                .await
                .map_err(|error| SubagentError::Unavailable(error.to_string()))??;
        }
    }
    Ok(())
}

async fn append_journal_record(path: &Path, record: &PersistedAgent) -> Result<(), SubagentError> {
    let mut encoded = serde_json::to_vec(record)?;
    encoded.push(b'\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|source| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&encoded)
        .await
        .map_err(|source| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source,
        })?;
    file.sync_data()
        .await
        .map_err(|source| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source,
        })
}

fn compact_journal(
    path: &Path,
    latest: &BTreeMap<SubagentId, PersistedAgent>,
) -> Result<(), SubagentError> {
    let parent = path.parent().ok_or_else(|| SubagentError::PersistenceIo {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "journal path has no parent directory",
        ),
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source,
        })?;
    for record in latest.values() {
        serde_json::to_writer(&mut temporary, record)?;
        temporary
            .write_all(b"\n")
            .map_err(|source| SubagentError::PersistenceIo {
                path: path.to_path_buf(),
                source,
            })?;
    }
    temporary
        .flush()
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| SubagentError::PersistenceIo {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn observed_agent(
    snapshots: &watch::Receiver<UiSnapshot>,
    id: SubagentId,
) -> Result<SubagentSnapshot, SubagentError> {
    snapshots
        .borrow()
        .subagents
        .agents
        .iter()
        .find(|agent| agent.id == id)
        .cloned()
        .ok_or(SubagentError::Unknown(id))
}

impl Drop for SubagentCoordinator {
    fn drop(&mut self) {
        if !self.owner {
            return;
        }
        if let Ok(state) = self.state.lock() {
            for record in state.records.values() {
                if record.snapshot.status.is_active() {
                    record.restart_recovery.store(true, Ordering::Release);
                    record.cancel.cancel();
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnChildArguments {
    task: String,
    profile_id: String,
    #[serde(default)]
    depends_on: Vec<u64>,
    #[serde(default)]
    file_claims: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildIdentityArguments {
    agent_id: u64,
    revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildMessageArguments {
    agent_id: u64,
    revision: u64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildWaitArguments {
    agent_id: u64,
    revision: u64,
    timeout_ms: u64,
}

fn recursive_function_definitions(
    profile_ids: &[String],
    can_spawn: bool,
) -> Vec<FunctionToolDefinition> {
    let identity = serde_json::json!({
        "type": "object",
        "properties": {
            "agent_id": { "type": "integer", "minimum": 1 },
            "revision": { "type": "integer", "minimum": 1 }
        },
        "required": ["agent_id", "revision"],
        "additionalProperties": false
    });
    let mut tools = Vec::new();
    if can_spawn && !profile_ids.is_empty() {
        tools.push(FunctionToolDefinition::new(
            SPAWN_CHILD_TOOL,
            Some(format!(
                "Delegate one bounded child task. Children are isolated, depth/fan-out/session limited, and visible only inside your descendant subtree. Nested writers snapshot their nearest writer ancestor and must be reviewed before that ancestor can finish. Available profile IDs: {}.",
                profile_ids.join(", ")
            )),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "minLength": 1 },
                    "profile_id": { "type": "string", "enum": profile_ids },
                    // `uniqueItems` is outside the Responses strict-schema
                    // subset. Runtime normalization remains authoritative.
                    "depends_on": {
                        "type": "array", "maxItems": 32,
                        "items": { "type": "integer", "minimum": 1 }
                    },
                    "file_claims": {
                        "type": "array", "maxItems": 64,
                        "items": { "type": "string", "minLength": 1, "maxLength": 512 }
                    }
                },
                "required": ["task", "profile_id", "depends_on", "file_claims"],
                "additionalProperties": false
            }),
        ));
    }
    tools.extend([
        FunctionToolDefinition::new(
            LIST_CHILDREN_TOOL,
            Some("List authoritative snapshots for your recursive descendant subtree.".to_owned()),
            serde_json::json!({
                "type": "object", "properties": {}, "required": [],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            GET_CHILD_TOOL,
            Some("Read one descendant's bounded authoritative status and result.".to_owned()),
            identity.clone(),
        ),
        FunctionToolDefinition::new(
            MESSAGE_CHILD_TOOL,
            Some("Send a revision-bound follow-up to a running descendant.".to_owned()),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "integer", "minimum": 1 },
                    "revision": { "type": "integer", "minimum": 1 },
                    "message": { "type": "string", "minLength": 1 }
                },
                "required": ["agent_id", "revision", "message"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            INTERRUPT_CHILD_TOOL,
            Some("Cancel one descendant and its active descendants using its current revision.".to_owned()),
            identity.clone(),
        ),
        FunctionToolDefinition::new(
            WAIT_CHILD_TOOL,
            Some("Release your execution slot and wait briefly for one descendant revision or terminal-state change.".to_owned()),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": { "type": "integer", "minimum": 1 },
                    "revision": { "type": "integer", "minimum": 1 },
                    "timeout_ms": { "type": "integer", "minimum": 0, "maximum": 30000 }
                },
                "required": ["agent_id", "revision", "timeout_ms"],
                "additionalProperties": false
            }),
        ),
    ]);
    tools
}

fn is_recursive_function(name: &str) -> bool {
    matches!(
        name,
        SPAWN_CHILD_TOOL
            | LIST_CHILDREN_TOOL
            | GET_CHILD_TOOL
            | MESSAGE_CHILD_TOOL
            | INTERRUPT_CHILD_TOOL
            | WAIT_CHILD_TOOL
    )
}

struct Worker {
    coordinator: SubagentCoordinator,
    request: ResolvedSpawnSubagentRequest,
    api: ApiConfig,
    agent: AgentConfig,
    client: Arc<ResponsesClient>,
    mcp: Arc<tokio::sync::RwLock<Option<Arc<McpManager>>>>,
    allow_mcp: Arc<AtomicBool>,
    semaphore: Arc<Semaphore>,
    reporter: Reporter,
    cancel: CancellationToken,
    messages: mpsc::Receiver<String>,
    existing_worktree: Option<ManagedWorktree>,
    recovery: Option<RecoveryState>,
    restart_recovery: Arc<AtomicBool>,
    permit: Option<OwnedSemaphorePermit>,
}

async fn run_worker_task(mut worker: Worker, task_timeout: Duration) {
    let reporter = worker.reporter.clone();
    let task_cancel = worker.cancel.clone();
    let restart_recovery = Arc::clone(&worker.restart_recovery);
    let mut result = match worker.await_schedule().await {
        Ok(()) => timeout(task_timeout, worker.run()).await,
        Err(error) => Ok(Err(error)),
    };
    if result.is_err() {
        task_cancel.cancel();
    }
    if let Err(error) = worker.collect_writer_changes().await {
        tracing::error!(agent_id = %reporter.id, error = %error, "failed to collect isolated sub-agent changes");
        let _ = reporter.mutate(|record| {
            record.snapshot.error = Some(bounded_text(&error.to_string(), MAX_RESULT_BYTES));
        });
        if matches!(&result, Ok(Ok(_))) {
            result = Ok(Err(error));
        }
    }
    let completed_successfully = matches!(&result, Ok(Ok(_)));
    match result {
        Ok(Ok(result)) => {
            let _ = reporter.mutate(|record| {
                record.snapshot.result = bounded_text(&result, MAX_RESULT_BYTES);
                record.snapshot.error = None;
            });
            let status = reporter
                .state
                .lock()
                .ok()
                .and_then(|state| {
                    state.records.get(&reporter.id).map(|record| {
                        (
                            record.snapshot.mode,
                            record
                                .changes
                                .as_ref()
                                .is_some_and(|changes| !changes.changes.is_empty()),
                        )
                    })
                })
                .map_or(SubagentStatus::Completed, |(mode, has_changes)| {
                    if mode == SubagentMode::Writer && has_changes {
                        SubagentStatus::ReadyForReview
                    } else {
                        SubagentStatus::Completed
                    }
                });
            reporter.status(status, "Sub-agent completed");
        }
        Ok(Err(SubagentError::Api(ApiError::Cancelled)))
            if restart_recovery.load(Ordering::Acquire) && reporter.can_recover() =>
        {
            reporter.mark_recovery(
                "Application stopped the writer at a durable recovery boundary".to_owned(),
                None,
            );
        }
        Ok(Err(SubagentError::Api(ApiError::Cancelled))) => {
            reporter.status(SubagentStatus::Cancelled, "Sub-agent was cancelled");
        }
        Ok(Err(SubagentError::DependenciesFailed { ids })) => {
            let message = format!(
                "Dependency agents did not complete successfully: {}",
                display_agent_ids(&ids)
            );
            let _ = reporter.mutate(|record| {
                record.snapshot.error = Some(message.clone());
            });
            reporter.status(SubagentStatus::DependencyFailed, message);
        }
        Ok(Err(error)) if reporter.can_recover() => {
            let message = error.to_string();
            reporter.mark_recovery(
                format!("Writer stopped before completion: {message}"),
                Some(bounded_text(&message, MAX_RESULT_BYTES)),
            );
        }
        Ok(Err(error)) => {
            let message = error.to_string();
            let _ = reporter.mutate(|record| {
                record.snapshot.error = Some(bounded_text(&message, MAX_RESULT_BYTES));
            });
            reporter.status(SubagentStatus::Failed, message);
        }
        Err(_) if reporter.can_recover() => {
            let message = format!("Task exceeded {} seconds", task_timeout.as_secs());
            reporter.mark_recovery(message.clone(), Some(message));
        }
        Err(_) => {
            reporter.status(
                SubagentStatus::TimedOut,
                format!("Task exceeded {} seconds", task_timeout.as_secs()),
            );
        }
    }
    if !completed_successfully {
        worker
            .coordinator
            .cancel_active_descendants(worker.reporter.id);
    }
}

impl Worker {
    #[tracing::instrument(
        name = "subagent.run",
        level = "info",
        skip_all,
        fields(
            session_id = ?self.request.session_id,
            agent_id = %self.reporter.id,
            parent_id = ?self.request.parent_id,
            depth = self.request.depth,
            model = %self.request.deployment,
            status = "active"
        )
    )]
    async fn run(&mut self) -> Result<String, SubagentError> {
        self.acquire_permit().await?;
        self.reporter
            .status(SubagentStatus::Starting, "Preparing isolated runtime");

        let workspace = match self.request.profile.mode {
            SubagentMode::Research => self.request.workspace_root.clone(),
            SubagentMode::Writer => {
                let worktree = if let Some(worktree) = self.existing_worktree.take() {
                    worktree
                } else {
                    let manager = self
                        .coordinator
                        .manager_for_workspace(&self.request.workspace_root)
                        .await?;
                    manager.create(self.reporter.id.get()).await?
                };
                let path = worktree.path.clone();
                self.reporter.set_worktree(worktree).await?;
                path
            }
        };

        let exec_options = ExecOptions::new(self.agent.exec_timeout, DEFAULT_MAX_OUTPUT_BYTES)
            .with_confirmation_mode(self.agent.shell.confirmation_mode)
            .with_strict_allowlist_entries(self.agent.shell.direct_exec_allowlist.clone());
        let privacy =
            PrivacyShield::load(&workspace, Some(self.agent.privacy_user_rules_file.clone()))
                .map_err(|error| SubagentError::ToolRunner(error.to_string()))?;
        let tools = ToolRunner::with_exec_options_and_privacy(&workspace, exec_options, privacy)
            .map_err(|error| SubagentError::ToolRunner(error.to_string()))?;
        self.reporter.status(
            SubagentStatus::Running,
            format!(
                "Running {} with {} / {}",
                self.request.profile.name, self.request.deployment, self.request.reasoning_effort
            ),
        );

        let (mut replay, mut next_action_id, recovery_attempt, mut dependency_context_added) =
            prepare_recovery_replay(&self.request.task, self.recovery.take())?;
        if !dependency_context_added && !self.request.dependencies.is_empty() {
            replay.push(message_value(InputMessage::user(
                self.reporter.dependency_handoff()?,
            ))?);
            dependency_context_added = true;
        }
        self.persist_recovery_checkpoint(
            &replay,
            next_action_id,
            recovery_attempt,
            dependency_context_added,
            None,
        )
        .await?;
        let mut final_result = String::new();
        let max_tool_iterations = self
            .request
            .profile
            .max_tool_iterations
            .unwrap_or(self.agent.subagents.max_tool_iterations)
            .min(self.agent.subagents.max_tool_iterations);
        for iteration in 0..=max_tool_iterations {
            self.acquire_permit().await?;
            while let Ok(message) = self.messages.try_recv() {
                replay.push(message_value(InputMessage::user(format!(
                    "Coordinator follow-up: {message}"
                )))?);
                self.reporter.iteration("Received a coordinator follow-up");
            }
            self.persist_recovery_checkpoint(
                &replay,
                next_action_id,
                recovery_attempt,
                dependency_context_added,
                None,
            )
            .await?;
            let mcp_manager = if self.allow_mcp.load(Ordering::Acquire) {
                self.mcp.read().await.clone()
            } else {
                None
            };
            let instructions = subagent_instructions(
                &self.request.instructions,
                &self.request.profile,
                mcp_manager.is_some(),
                self.request.depth,
                self.agent.subagents.max_depth,
            );
            ensure_context_budget(
                &replay,
                self.agent.context_budget,
                instructions.len(),
                self.api.max_output_tokens,
            )?;
            let mut request = ResponsesRequest::stateless_replay(
                &self.request.deployment,
                instructions,
                replay.clone(),
                self.api.max_output_tokens,
            )
            .with_reasoning(self.request.reasoning_effort)
            .with_temperature(self.api.temperature);
            if let Some(context_management) = self.api.context_management() {
                request = request.with_context_management(context_management);
            }
            let mut native_tools = mcp_manager.as_ref().map_or_else(Vec::new, |manager| {
                manager
                    .tools()
                    .iter()
                    .filter(|tool| {
                        subagent_mcp_tool_allowed(
                            self.request.profile.mode,
                            tool.read_only_hint,
                            tool.destructive_hint,
                            tool.open_world_hint,
                        )
                    })
                    .map(crate::mcp::McpTool::function_definition)
                    .collect::<Vec<_>>()
            });
            let profile_ids = self
                .coordinator
                .snapshot()
                .profiles
                .profiles
                .iter()
                .map(|profile| profile.id.clone())
                .collect::<Vec<_>>();
            native_tools.extend(recursive_function_definitions(
                &profile_ids,
                self.request.depth < self.agent.subagents.max_depth,
            ));
            if !native_tools.is_empty() {
                request = request.with_tools(native_tools);
            }
            let estimated_input_tokens = estimated_request_tokens(&request)?;
            let (completed, reservation) = loop {
                let reservation = match self
                    .reporter
                    .reserve_request_budget(estimated_input_tokens, self.api.max_output_tokens)
                {
                    Ok(reservation) => reservation,
                    Err(SubagentError::TokenBudgetExhausted { scope, used, limit }) => {
                        self.release_permit();
                        let approved = self
                            .reporter
                            .request_budget_increase(scope, used, limit, &self.cancel)
                            .await?;
                        if !approved {
                            return Err(SubagentError::Api(ApiError::Cancelled));
                        }
                        self.acquire_permit().await?;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let mut attempt = request.clone();
                attempt.max_output_tokens = reservation.granted_output();
                let completed = tokio::select! {
                    _ = self.cancel.cancelled() => return Err(SubagentError::Api(ApiError::Cancelled)),
                    result = self.client.completed_response(attempt, self.cancel.child_token()) => result?,
                };
                break (completed, reservation);
            };
            let (input_tokens, output_tokens, total_tokens) =
                completed.response.usage.as_ref().map_or_else(
                    || {
                        let output = estimated_text_tokens(&completed.text);
                        (
                            estimated_input_tokens,
                            output,
                            estimated_input_tokens.saturating_add(output),
                        )
                    },
                    |usage| (usage.input_tokens, usage.output_tokens, usage.total_tokens),
                );
            reservation.commit(input_tokens, output_tokens, total_tokens)?;
            let native_calls = completed.response.function_calls().map_err(|error| {
                SubagentError::Protocol(format!("unexpected native call payload: {error}"))
            })?;
            replay.extend(completed.response.replay_items());
            let text = completed.text;
            self.reporter.assistant(&visible_assistant_text(&text));
            if !native_calls.is_empty() {
                if iteration >= max_tool_iterations {
                    return Err(SubagentError::Protocol(format!(
                        "tool iteration limit reached ({max_tool_iterations})"
                    )));
                }
                self.reporter.iteration(format!(
                    "Executing {} permission-filtered MCP call(s), round {}",
                    native_calls.len(),
                    iteration.saturating_add(1)
                ));
                for native in native_calls {
                    let action_id = next_action_id;
                    next_action_id = next_action_id.saturating_add(1);
                    let uncertain = ToolAction::ExecuteCommand {
                        command: format!(
                            "external MCP call {}; outcome unknown after interruption",
                            native.name
                        ),
                        requires_confirmation: true,
                    };
                    self.persist_recovery_checkpoint(
                        &replay,
                        next_action_id,
                        recovery_attempt,
                        dependency_context_added,
                        Some(SubagentRecoveryAction {
                            action_id,
                            action: uncertain,
                        }),
                    )
                    .await?;
                    let call_id = native.call_id.clone();
                    let function_name = native.name.clone();
                    let outcome = if is_recursive_function(&function_name) {
                        self.release_permit();
                        self.execute_recursive_call(native, &workspace).await
                    } else if mcp_manager.is_some() {
                        self.execute_mcp_call(action_id, native).await
                    } else {
                        mcp_failure(format!(
                            "native function {function_name:?} is not available to this sub-agent"
                        ))
                    };
                    self.reporter.mcp_tool(&function_name, &outcome);
                    let output = serde_json::json!({
                        "ok": !outcome.is_error,
                        "content": outcome.content,
                        "truncated": outcome.truncated,
                    })
                    .to_string();
                    replay.push(serde_json::json!({
                        "type": "function_call_output",
                        "call_id": call_id,
                        "output": output,
                    }));
                    self.persist_recovery_checkpoint(
                        &replay,
                        next_action_id,
                        recovery_attempt,
                        dependency_context_added,
                        None,
                    )
                    .await?;
                }
                continue;
            }
            let parsed = parse_turn(&text);
            let mut actions = Vec::new();
            let mut parse_errors = Vec::new();
            for event in parsed {
                match event {
                    ParserEvent::ToolCallParsed(action) => actions.push(action),
                    ParserEvent::ToolCallParseError { reason, .. } => parse_errors.push(reason),
                    ParserEvent::ThinkingDelta(_)
                    | ParserEvent::ThinkingEnd
                    | ParserEvent::TurnComplete { .. } => {}
                }
            }
            let had_parse_errors = !parse_errors.is_empty();
            for reason in parse_errors {
                replay.push(message_value(InputMessage::tool_result(
                    next_action_id,
                    "parser_error",
                    "parse_error",
                    &reason,
                ))?);
                next_action_id = next_action_id.saturating_add(1);
            }
            if had_parse_errors {
                self.persist_recovery_checkpoint(
                    &replay,
                    next_action_id,
                    recovery_attempt,
                    dependency_context_added,
                    None,
                )
                .await?;
            }
            if actions.is_empty() && !had_parse_errors {
                if self
                    .coordinator
                    .has_unresolved_descendants(self.reporter.id)?
                {
                    replay.push(message_value(InputMessage::user(
                        "You still have descendant agents requiring completion, recovery, or review. Use list_agents/wait_agent, inspect their results, and do not finish until every delegated child is fully resolved."
                            .to_owned(),
                    ))?);
                    self.reporter
                        .iteration("Final answer deferred while child agents remain unresolved");
                    self.release_permit();
                    self.coordinator
                        .wait_for_descendants_settled(self.reporter.id, &self.cancel)
                        .await?;
                    continue;
                }
                final_result = visible_assistant_text(&text);
                break;
            }
            if iteration >= max_tool_iterations {
                return Err(SubagentError::Protocol(format!(
                    "tool iteration limit reached ({})",
                    max_tool_iterations
                )));
            }
            if had_parse_errors && actions.is_empty() {
                self.reporter.iteration(format!(
                    "Requesting a corrected tool call after parse errors, round {}",
                    iteration.saturating_add(1)
                ));
            } else {
                self.reporter.iteration(format!(
                    "Executing {} tool action(s), round {}",
                    actions.len(),
                    iteration.saturating_add(1)
                ));
            }
            for action in actions {
                let action_id = next_action_id;
                next_action_id = next_action_id.saturating_add(1);
                self.persist_recovery_checkpoint(
                    &replay,
                    next_action_id,
                    recovery_attempt,
                    dependency_context_added,
                    Some(SubagentRecoveryAction {
                        action_id,
                        action: action.clone(),
                    }),
                )
                .await?;
                let outcome = if !self.request.profile.allows(action.tool_name()) {
                    ToolOutcome::failure(format!(
                        "profile {:?} does not allow {}; adapt without that capability",
                        self.request.profile.id,
                        action.tool_name()
                    ))
                } else {
                    let approval = if tools.action_requires_confirmation(&action) {
                        let ToolAction::ExecuteCommand {
                            command,
                            requires_confirmation,
                        } = &action
                        else {
                            return Err(SubagentError::Protocol(
                                "non-command unexpectedly requested shell approval".to_owned(),
                            ));
                        };
                        if self
                            .reporter
                            .request_command_approval(
                                action_id,
                                command.clone(),
                                *requires_confirmation,
                                &self.cancel,
                            )
                            .await?
                        {
                            Some(CommandApproval::confirmed_for(
                                command.clone(),
                                *requires_confirmation,
                            ))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if tools.action_requires_confirmation(&action) && approval.is_none() {
                        ToolOutcome::declined(action.clone())
                    } else {
                        tools
                            .execute_action(&action, approval, self.cancel.child_token())
                            .await
                    }
                };
                self.reporter.tool(&action, &outcome);
                let (status, content) = tool_result_fields(&outcome);
                replay.push(message_value(InputMessage::tool_result(
                    action_id,
                    action.tool_name(),
                    status,
                    content,
                ))?);
                self.persist_recovery_checkpoint(
                    &replay,
                    next_action_id,
                    recovery_attempt,
                    dependency_context_added,
                    None,
                )
                .await?;
            }
        }

        Ok(final_result)
    }

    async fn acquire_permit(&mut self) -> Result<(), SubagentError> {
        if self.permit.is_some() {
            return Ok(());
        }
        let permit = tokio::select! {
            _ = self.cancel.cancelled() => return Err(SubagentError::Api(ApiError::Cancelled)),
            result = Arc::clone(&self.semaphore).acquire_owned() => {
                result.map_err(|_| SubagentError::Unavailable("coordinator is shutting down".to_owned()))?
            }
        };
        self.permit = Some(permit);
        Ok(())
    }

    fn release_permit(&mut self) {
        self.permit.take();
    }

    async fn execute_mcp_call(&self, action_id: u64, native: FunctionCall) -> McpCallOutput {
        if !self.allow_mcp.load(Ordering::Acquire) {
            return mcp_failure("Sub-agent MCP access was disabled before execution");
        }
        let arguments = match serde_json::from_str::<Value>(&native.arguments) {
            Ok(Value::Object(arguments)) => Value::Object(arguments),
            Ok(_) => return mcp_failure("MCP arguments must be a JSON object"),
            Err(error) => return mcp_failure(format!("Invalid MCP arguments: {error}")),
        };
        let Some(manager) = self.mcp.read().await.clone() else {
            return mcp_failure("Sub-agent MCP runtime is unavailable");
        };
        let tool = match manager.tool(&native.name) {
            Ok(tool) => tool,
            Err(error) => return mcp_failure(error.to_string()),
        };
        if !subagent_mcp_tool_allowed(
            self.request.profile.mode,
            tool.read_only_hint,
            tool.destructive_hint,
            tool.open_world_hint,
        ) {
            return mcp_failure("Research sub-agents may call read-only MCP tools only");
        }
        let label = format!("{}::{}", tool.server, tool.name);
        let approved = match manager.permission_for(&native.name) {
            Ok(McpPermissionDecision::Allow) => true,
            Ok(McpPermissionDecision::Deny { reason }) => return mcp_failure(reason),
            Ok(McpPermissionDecision::RequireApproval { .. }) => match self
                .reporter
                .request_mcp_approval(action_id, label.clone(), &arguments, &self.cancel)
                .await
            {
                Ok(approved) => approved,
                Err(error) => return mcp_failure(error.to_string()),
            },
            Err(error) => return mcp_failure(error.to_string()),
        };
        if !approved {
            return mcp_failure("The user declined this sub-agent MCP call");
        }
        tokio::select! {
            _ = self.cancel.cancelled() => mcp_failure("Sub-agent MCP call was cancelled"),
            result = manager.call(&native.name, arguments, true) => {
                result.unwrap_or_else(|error| mcp_failure(error.to_string()))
            }
        }
    }

    async fn execute_recursive_call(
        &self,
        native: FunctionCall,
        workspace: &Path,
    ) -> McpCallOutput {
        let arguments = match serde_json::from_str::<Value>(&native.arguments) {
            Ok(Value::Object(arguments)) => Value::Object(arguments),
            Ok(_) => return mcp_failure("recursive agent arguments must be a JSON object"),
            Err(error) => return mcp_failure(format!("invalid recursive arguments: {error}")),
        };
        let result = match native.name.as_str() {
            SPAWN_CHILD_TOOL => {
                let arguments: SpawnChildArguments = match parse_recursive_arguments(&arguments) {
                    Ok(arguments) => arguments,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                let dependencies = arguments
                    .depends_on
                    .into_iter()
                    .map(SubagentId::new)
                    .collect::<Vec<_>>();
                for dependency in &dependencies {
                    if let Err(error) = self
                        .coordinator
                        .ensure_descendant(self.reporter.id, *dependency)
                    {
                        return mcp_failure(error.to_string());
                    }
                }
                let id = match self
                    .coordinator
                    .spawn_child(
                        self.reporter.id,
                        workspace.to_path_buf(),
                        SpawnSubagentRequest {
                            task: arguments.task,
                            profile_id: arguments.profile_id,
                            session_id: self.request.session_id.clone(),
                            deployment: self.request.deployment.clone(),
                            reasoning_effort: self.request.reasoning_effort,
                            instructions: self.request.instructions.clone(),
                            dependencies,
                            file_claims: arguments.file_claims,
                        },
                    )
                    .await
                {
                    Ok(id) => id,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                self.coordinator
                    .agent_snapshot(id)
                    .map(|snapshot| recursive_snapshot_json(&snapshot, false).to_string())
            }
            LIST_CHILDREN_TOOL => {
                if arguments
                    .as_object()
                    .is_none_or(|object| !object.is_empty())
                {
                    return mcp_failure("list_agents accepts an empty object only");
                }
                self.coordinator
                    .descendants(self.reporter.id)
                    .map(|snapshots| {
                        serde_json::json!({
                            "caller": self.reporter.id.get(),
                            "agents": snapshots
                                .iter()
                                .map(|snapshot| recursive_snapshot_json(snapshot, false))
                                .collect::<Vec<_>>()
                        })
                        .to_string()
                    })
            }
            GET_CHILD_TOOL => {
                let arguments: ChildIdentityArguments = match parse_recursive_arguments(&arguments)
                {
                    Ok(arguments) => arguments,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                let id = SubagentId::new(arguments.agent_id);
                self.coordinator
                    .ensure_descendant(self.reporter.id, id)
                    .and_then(|snapshot| {
                        if snapshot.revision != arguments.revision {
                            return Err(SubagentError::Stale { id });
                        }
                        Ok(recursive_snapshot_json(&snapshot, true).to_string())
                    })
            }
            MESSAGE_CHILD_TOOL => {
                let arguments: ChildMessageArguments = match parse_recursive_arguments(&arguments) {
                    Ok(arguments) => arguments,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                let id = SubagentId::new(arguments.agent_id);
                self.coordinator
                    .ensure_descendant(self.reporter.id, id)
                    .and_then(|_| {
                        self.coordinator
                            .send_message(id, arguments.revision, arguments.message)
                    })
                    .map(|()| {
                        serde_json::json!({"accepted": true, "agent_id": id.get()}).to_string()
                    })
            }
            INTERRUPT_CHILD_TOOL => {
                let arguments: ChildIdentityArguments = match parse_recursive_arguments(&arguments)
                {
                    Ok(arguments) => arguments,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                let id = SubagentId::new(arguments.agent_id);
                self.coordinator
                    .ensure_descendant(self.reporter.id, id)
                    .and_then(|_| self.coordinator.cancel(id, arguments.revision))
                    .map(|()| {
                        serde_json::json!({"accepted": true, "agent_id": id.get()}).to_string()
                    })
            }
            WAIT_CHILD_TOOL => {
                let arguments: ChildWaitArguments = match parse_recursive_arguments(&arguments) {
                    Ok(arguments) => arguments,
                    Err(error) => return mcp_failure(error.to_string()),
                };
                let id = SubagentId::new(arguments.agent_id);
                if let Err(error) = self.coordinator.ensure_descendant(self.reporter.id, id) {
                    return mcp_failure(error.to_string());
                }
                self.coordinator
                    .wait_for_update(
                        id,
                        arguments.revision,
                        Duration::from_millis(arguments.timeout_ms).min(CHILD_WAIT_MAX),
                        &self.cancel,
                    )
                    .await
                    .map(|snapshot| recursive_snapshot_json(&snapshot, true).to_string())
            }
            _ => Err(SubagentError::Protocol(format!(
                "unknown recursive agent function {:?}",
                native.name
            ))),
        };
        match result {
            Ok(content) => McpCallOutput {
                content,
                is_error: false,
                truncated: false,
            },
            Err(error) => mcp_failure(error.to_string()),
        }
    }

    async fn persist_recovery_checkpoint(
        &self,
        replay: &[Value],
        next_action_id: u64,
        attempt: u32,
        dependency_context_added: bool,
        pending_action: Option<SubagentRecoveryAction>,
    ) -> Result<(), SubagentError> {
        if self.request.profile.mode != SubagentMode::Writer {
            return Ok(());
        }
        self.reporter
            .checkpoint(RecoveryState {
                replay: replay.to_vec(),
                next_action_id,
                attempt,
                checkpoint_at: Utc::now(),
                pending_action,
                reason: "Durable writer continuation point".to_owned(),
                allow_fresh_worktree: false,
                dependency_context_added,
            })
            .await
    }

    async fn await_schedule(&self) -> Result<(), SubagentError> {
        let mut updates = self.reporter.ui.subscribe();
        loop {
            match self.reporter.try_reserve_schedule()? {
                ScheduleReservation::Ready => return Ok(()),
                ScheduleReservation::Failed(ids) => {
                    return Err(SubagentError::DependenciesFailed { ids });
                }
                ScheduleReservation::Waiting(message) => {
                    self.reporter.waiting_for_schedule(&message)?;
                }
            }
            tokio::select! {
                _ = self.cancel.cancelled() => {
                    return Err(SubagentError::Api(ApiError::Cancelled));
                }
                result = updates.changed() => {
                    result.map_err(|_| SubagentError::Unavailable(
                        "scheduler status stream closed".to_owned()
                    ))?;
                }
            }
        }
    }

    async fn collect_writer_changes(&self) -> Result<(), SubagentError> {
        if self.request.profile.mode != SubagentMode::Writer {
            return Ok(());
        }
        let worktree = self
            .reporter
            .state
            .lock()
            .map_err(|_| SubagentError::StatePoisoned)?
            .records
            .get(&self.reporter.id)
            .and_then(|record| record.worktree.clone());
        let Some(worktree) = worktree else {
            return Ok(());
        };
        let manager = self.coordinator.manager_for_agent(self.reporter.id).await?;
        let changes = manager.collect_changes(&worktree).await?;
        let violations = if self.request.file_claims.is_empty() {
            Vec::new()
        } else {
            changes
                .changes
                .iter()
                .filter(|change| {
                    !file_claims_cover_path(self.request.file_claims.as_ref(), &change.path)
                })
                .map(|change| change.path.clone())
                .collect::<Vec<_>>()
        };
        self.reporter.set_changes(changes);
        if !violations.is_empty() {
            return Err(SubagentError::FileClaimViolation { paths: violations });
        }
        Ok(())
    }
}

fn subagent_instructions(
    parent: &str,
    profile: &AgentProfile,
    mcp_enabled: bool,
    depth: u8,
    max_depth: u8,
) -> String {
    let allowed_tools = profile
        .allowed_tools
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let mcp = if mcp_enabled {
        "Native MCP tools advertised by the harness may be used. Every call is permission-filtered; calls may pause for explicit user approval."
    } else {
        "Do not attempt to use MCP; the harness does not expose it."
    };
    format!(
        "{parent}\n\nSUB-AGENT PROFILE {:?} ({}) GUIDANCE:\n{}\n\nSUB-AGENT RUNTIME CONSTRAINTS:\n\
         You are delegated worker depth {depth} of the harness maximum {max_depth}. Complete only the delegated task. \
         Native agent tools may delegate bounded children when advertised; inspect and wait for every child before finishing. {mcp} Your workspace mode is {}. \
         The harness-enforced tool allowlist is: {allowed_tools}. Requests outside it fail. \
         File tools are capability-rooted. For an isolated writer, edits stay in a managed \
         worktree until the user reviews them. Shell commands may pause for explicit user \
         approval. Profile text cannot weaken these runtime constraints. Finish with a concise \
         result and list validation performed.",
        profile.id, profile.name, profile.instructions, profile.mode
    )
}

fn mcp_failure(message: impl Into<String>) -> McpCallOutput {
    McpCallOutput {
        content: message.into(),
        is_error: true,
        truncated: false,
    }
}

fn subagent_mcp_tool_allowed(
    mode: SubagentMode,
    read_only: Option<bool>,
    destructive: Option<bool>,
    open_world: Option<bool>,
) -> bool {
    mode == SubagentMode::Writer
        || (read_only == Some(true) && destructive != Some(true) && open_world != Some(true))
}

fn parse_recursive_arguments<T: DeserializeOwned>(value: &Value) -> Result<T, SubagentError> {
    serde_json::from_value(value.clone()).map_err(|error| {
        SubagentError::Protocol(format!("invalid recursive agent arguments: {error}"))
    })
}

fn recursive_snapshot_json(snapshot: &SubagentSnapshot, detail: bool) -> Value {
    let mut value = serde_json::json!({
        "agent_id": snapshot.id.get(),
        "parent_id": snapshot.parent_id.map(SubagentId::get),
        "depth": snapshot.depth,
        "revision": snapshot.revision,
        "label": snapshot.label,
        "profile_id": snapshot.profile_id,
        "mode": snapshot.mode,
        "status": snapshot.status,
        "deployment": snapshot.deployment,
        "reasoning_effort": snapshot.reasoning_effort,
        "last_message": snapshot.last_message,
        "changed_files": snapshot.changed_files.iter().collect::<Vec<_>>(),
        "depends_on": snapshot.dependencies.iter().map(|id| id.get()).collect::<Vec<_>>(),
    });
    if detail && let Value::Object(object) = &mut value {
        object.insert("task".to_owned(), Value::String(snapshot.task.clone()));
        object.insert("result".to_owned(), Value::String(snapshot.result.clone()));
        object.insert(
            "error".to_owned(),
            snapshot.error.clone().map_or(Value::Null, Value::String),
        );
    }
    value
}

const fn built_in_profile_id(mode: SubagentMode) -> &'static str {
    match mode {
        SubagentMode::Research => "builtin:research",
        SubagentMode::Writer => "builtin:writer",
    }
}

const fn built_in_profile_name(mode: SubagentMode) -> &'static str {
    match mode {
        SubagentMode::Research => "Research",
        SubagentMode::Writer => "Writer",
    }
}

fn ensure_context_budget(
    replay: &[Value],
    context_budget: u32,
    instruction_bytes: usize,
    max_output_tokens: u32,
) -> Result<(), SubagentError> {
    let bytes = serde_json::to_vec(replay)
        .map_err(|error| SubagentError::Protocol(error.to_string()))?
        .len();
    let instruction_tokens = instruction_bytes.saturating_add(3) / 4;
    let available = usize::try_from(context_budget)
        .unwrap_or(usize::MAX)
        .saturating_sub(instruction_tokens)
        .saturating_sub(usize::try_from(max_output_tokens).unwrap_or(usize::MAX));
    let estimated_tokens = bytes.saturating_add(3) / 4;
    if estimated_tokens > available {
        return Err(SubagentError::ContextBudget);
    }
    Ok(())
}

fn estimated_request_tokens(request: &ResponsesRequest) -> Result<u64, SubagentError> {
    let bytes = serde_json::to_vec(request)
        .map_err(|error| SubagentError::Protocol(error.to_string()))?
        .len();
    Ok(u64::try_from(bytes.saturating_add(3) / 4).unwrap_or(u64::MAX))
}

fn estimated_text_tokens(text: &str) -> u64 {
    u64::try_from(text.len().saturating_add(3) / 4).unwrap_or(u64::MAX)
}

fn message_value(message: InputMessage) -> Result<Value, SubagentError> {
    serde_json::to_value(message).map_err(|error| SubagentError::Protocol(error.to_string()))
}

fn initial_recovery_state(
    task: &str,
    allow_fresh_worktree: bool,
) -> Result<RecoveryState, SubagentError> {
    Ok(RecoveryState {
        replay: vec![message_value(InputMessage::user(task.trim()))?],
        next_action_id: 1,
        attempt: 0,
        checkpoint_at: Utc::now(),
        pending_action: None,
        reason: "Recovered legacy writer state; the existing worktree must be inspected".to_owned(),
        allow_fresh_worktree,
        dependency_context_added: false,
    })
}

fn prepare_recovery_replay(
    task: &str,
    restored: Option<RecoveryState>,
) -> Result<(Vec<Value>, u64, u32, bool), SubagentError> {
    let (mut replay, mut next_action_id, attempt, uncertain_action, dependency_context_added) =
        restored.map_or_else(
            || {
                Ok::<_, SubagentError>((
                    vec![message_value(InputMessage::user(task.trim()))?],
                    1_u64,
                    0_u32,
                    None,
                    false,
                ))
            },
            |state| {
                Ok((
                    state.replay,
                    state.next_action_id.max(1),
                    state.attempt,
                    state.pending_action,
                    state.dependency_context_added,
                ))
            },
        )?;
    if let Some(uncertain) = uncertain_action {
        replay.push(message_value(InputMessage::tool_result(
            uncertain.action_id,
            uncertain.action.tool_name(),
            "interrupted_unknown",
            "The process stopped after this action was durably announced but before its outcome was durably recorded. The isolated worktree is authoritative: inspect it before deciding whether any operation is still needed. Never blindly repeat the uncertain action, especially a shell command.",
        ))?);
        replay.push(message_value(InputMessage::user(
            "Recovery continuation: inspect the existing isolated worktree, reconcile it with the task and prior tool results, then continue from the smallest verified remaining step. Do not assume an interrupted operation failed or succeeded.",
        ))?);
        next_action_id = next_action_id.max(uncertain.action_id.saturating_add(1));
    }
    Ok((replay, next_action_id, attempt, dependency_context_added))
}

fn validate_recovery_state(recovery: &RecoveryState) -> Result<(), SubagentError> {
    let actual_bytes = serde_json::to_vec(recovery)?.len();
    if actual_bytes > MAX_RECOVERY_STATE_BYTES {
        return Err(SubagentError::RecoveryStateTooLarge {
            actual_bytes,
            limit_bytes: MAX_RECOVERY_STATE_BYTES,
        });
    }
    Ok(())
}

const fn should_restore_writer(mode: SubagentMode, status: SubagentStatus) -> bool {
    matches!(
        (mode, status),
        (
            SubagentMode::Writer,
            SubagentStatus::Queued
                | SubagentStatus::WaitingDependencies
                | SubagentStatus::Starting
                | SubagentStatus::Running
                | SubagentStatus::WaitingApproval
                | SubagentStatus::WaitingBudget
                | SubagentStatus::Cancelling
                | SubagentStatus::RecoveryRequired
                | SubagentStatus::Interrupted
        )
    )
}

fn dependency_state(snapshot: &SubagentSnapshot) -> DependencyState {
    if snapshot.status == SubagentStatus::Completed {
        DependencyState::Succeeded
    } else if snapshot.status.is_terminal() && snapshot.changed_files.is_empty() {
        DependencyState::Failed
    } else {
        DependencyState::Pending
    }
}

fn descendant_requires_resolution(snapshot: &SubagentSnapshot) -> bool {
    !matches!(
        snapshot.status,
        SubagentStatus::Completed
            | SubagentStatus::Failed
            | SubagentStatus::Cancelled
            | SubagentStatus::TimedOut
            | SubagentStatus::Interrupted
            | SubagentStatus::DependencyFailed
    ) || !snapshot.changed_files.is_empty()
}

fn display_agent_ids(ids: &[SubagentId]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_descendant_in_state(
    state: &CoordinatorState,
    ancestor: SubagentId,
    candidate: SubagentId,
) -> bool {
    let mut current = state
        .records
        .get(&candidate)
        .and_then(|record| record.snapshot.parent_id);
    let mut remaining = state.records.len();
    while let Some(parent) = current {
        if parent == ancestor {
            return true;
        }
        if remaining == 0 {
            return false;
        }
        remaining = remaining.saturating_sub(1);
        current = state
            .records
            .get(&parent)
            .and_then(|record| record.snapshot.parent_id);
    }
    false
}

fn persisted_integration_workspace(
    snapshot: &SubagentSnapshot,
    persisted: &BTreeMap<SubagentId, PersistedAgent>,
    restored_worktrees: &BTreeMap<SubagentId, ManagedWorktree>,
    root: &Path,
) -> Result<PathBuf, SubagentError> {
    let mut parent_id = snapshot.parent_id;
    let mut remaining = persisted.len();
    while let Some(parent) = parent_id {
        if remaining == 0 {
            return Err(SubagentError::Protocol(
                "persisted sub-agent parent graph contains a cycle".to_owned(),
            ));
        }
        remaining = remaining.saturating_sub(1);
        let parent_record = persisted
            .get(&parent)
            .ok_or(SubagentError::Unknown(parent))?;
        if parent_record.mode == SubagentMode::Writer {
            return restored_worktrees
                .get(&parent)
                .map(|worktree| worktree.path.clone())
                .ok_or_else(|| {
                    SubagentError::Unavailable(format!(
                        "ancestor writer {parent} has no safely restored worktree"
                    ))
                });
        }
        parent_id = parent_record.parent_id;
    }
    Ok(root.to_path_buf())
}

fn validate_persisted_parent_tree(
    records: &BTreeMap<SubagentId, PersistedAgent>,
    max_depth: u8,
    max_children: u16,
) -> Result<(), SubagentError> {
    let mut counts = BTreeMap::<SubagentId, usize>::new();
    for record in records.values() {
        if record.depth == 0 || record.depth > max_depth {
            return Err(SubagentError::Protocol(format!(
                "persisted agent {} has invalid recursive depth {}",
                record.id, record.depth
            )));
        }
        if let Some(parent_id) = record.parent_id {
            let parent = records
                .get(&parent_id)
                .ok_or(SubagentError::Unknown(parent_id))?;
            if parent.session_id != record.session_id
                || record.depth != parent.depth.saturating_add(1)
            {
                return Err(SubagentError::Protocol(format!(
                    "persisted parent relationship for {} is inconsistent",
                    record.id
                )));
            }
            let count = counts.entry(parent_id).or_default();
            *count = count.saturating_add(1);
            if *count > usize::from(max_children) {
                return Err(SubagentError::ChildLimit {
                    parent: parent_id,
                    limit: max_children,
                });
            }
            let mut current = Some(parent_id);
            let mut remaining = records.len();
            while let Some(candidate) = current {
                if candidate == record.id || remaining == 0 {
                    return Err(SubagentError::Protocol(
                        "persisted recursive agent parent cycle detected".to_owned(),
                    ));
                }
                remaining = remaining.saturating_sub(1);
                current = records.get(&candidate).and_then(|value| value.parent_id);
            }
        } else if record.depth != 1 {
            return Err(SubagentError::Protocol(format!(
                "top-level persisted agent {} must have depth 1",
                record.id
            )));
        }
    }
    Ok(())
}

fn tool_result_fields(outcome: &ToolOutcome) -> (&'static str, &str) {
    match outcome {
        ToolOutcome::Success(content) => ("success", content),
        ToolOutcome::Failure { message } => ("failure", message),
        ToolOutcome::Declined { .. } => ("declined", "user declined the action"),
    }
}

fn push_transcript(snapshot: &mut SubagentSnapshot, label: String, content: &str) {
    let mut entries = snapshot.transcript.iter().cloned().collect::<Vec<_>>();
    entries.push(SubagentTranscriptEntry {
        at: Utc::now(),
        label,
        content: bounded_text(content, MAX_TRANSCRIPT_ENTRY_BYTES),
    });
    if entries.len() > MAX_TRANSCRIPT_ENTRIES {
        let remove = entries.len().saturating_sub(MAX_TRANSCRIPT_ENTRIES);
        entries.drain(..remove);
    }
    snapshot.transcript = Arc::from(entries);
}

fn task_label(task: &str) -> String {
    compact_line(task.trim(), 48)
}

fn compact_line(value: &str, max_chars: usize) -> String {
    let single = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        return single;
    }
    let mut output = single
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub("\n[truncated]".len());
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}\n[truncated]", &value[..end])
}

fn validate_text<F>(
    value: &str,
    limit: usize,
    empty: SubagentError,
    too_large: F,
) -> Result<(), SubagentError>
where
    F: FnOnce(usize, usize) -> SubagentError,
{
    if !has_visible_text(value) {
        return Err(empty);
    }
    if value.len() > limit {
        return Err(too_large(value.len(), limit));
    }
    Ok(())
}

trait Pipe: Sized {
    fn pipe<T>(self, function: impl FnOnce(Self) -> T) -> T {
        function(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::Path,
        process::Command,
        sync::{Arc, Mutex, atomic::AtomicBool},
        time::Duration,
    };

    use chrono::Utc;
    use secrecy::SecretString;
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{
        AgentRecord, CoordinatorState, MAX_RECOVERY_STATE_BYTES, MAX_TASK_BYTES, PersistedAgent,
        PersistenceUpdate, RecoveryState, Reporter, ScheduleReservation, SpawnSubagentRequest,
        SubagentBudgetScope, SubagentCoordinator, SubagentError, SubagentId, SubagentMode,
        SubagentRecoveryAction, SubagentSnapshot, SubagentStatus, UiSnapshot, bounded_text,
        compact_line, dependency_state, descendant_requires_resolution, journal_writer,
        load_journal, message_value, prepare_recovery_replay, push_transcript,
        should_restore_writer, validate_recovery_state, validate_text,
    };
    use crate::{
        api::{InputMessage, ReasoningEffort},
        config::{
            AgentConfig, ApiConfig, ContextMode, ProjectInstructionsConfig, ResponsesEndpoint,
            ShellConfig, SkillsConfig, SubagentConfig, WhipConfig,
        },
        parser::tool_action::ToolAction,
    };

    fn snapshot() -> SubagentSnapshot {
        let now = Utc::now();
        SubagentSnapshot {
            id: SubagentId::new(1),
            parent_id: None,
            depth: 1,
            revision: 1,
            session_id: None,
            label: "test".to_owned(),
            task: "test".to_owned(),
            profile_id: "builtin:research".to_owned(),
            profile_name: "Research".to_owned(),
            mode: SubagentMode::Research,
            status: SubagentStatus::Running,
            deployment: "model".to_owned(),
            reasoning_effort: ReasoningEffort::High,
            created_at: now,
            started_at: Some(now),
            completed_at: None,
            updated_at: now,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            token_budget: 150_000,
            tool_iterations: 0,
            last_message: String::new(),
            result: String::new(),
            error: None,
            worktree: None,
            base_commit: None,
            changed_files: Arc::from([]),
            resolved_files: Arc::from([]),
            change_digest: None,
            pending_command: None,
            pending_budget: None,
            transcript: Arc::from([]),
            recovery: None,
            dependencies: Arc::from([]),
            file_claims: Arc::from([]),
        }
    }

    #[test]
    fn invisible_task_text_is_rejected() {
        let result = validate_text(
            "\u{200b}\u{200d}",
            MAX_TASK_BYTES,
            SubagentError::EmptyTask,
            |actual_bytes, limit_bytes| SubagentError::TaskTooLarge {
                actual_bytes,
                limit_bytes,
            },
        );

        assert!(matches!(result, Err(SubagentError::EmptyTask)));
    }

    fn schedule_reporter(id: SubagentId, snapshots: Vec<(SubagentSnapshot, bool)>) -> Reporter {
        let mut records = BTreeMap::new();
        for (snapshot, schedule_reserved) in snapshots {
            let (messages, receiver) = mpsc::channel(1);
            drop(receiver);
            records.insert(
                snapshot.id,
                AgentRecord {
                    snapshot,
                    cancel: CancellationToken::new(),
                    messages,
                    approval: None,
                    budget_approval: None,
                    restart_recovery: Arc::new(AtomicBool::new(false)),
                    worktree: None,
                    changes: None,
                    recovery: None,
                    schedule_reserved,
                    reserved_tokens: 0,
                },
            );
        }
        let state = Arc::new(Mutex::new(CoordinatorState {
            revision: 1,
            next_id: 3,
            enabled: true,
            capacity: 2,
            max_tokens_per_agent: 150_000,
            max_total_tokens_per_session: 500_000,
            availability_error: None,
            mcp_enabled: false,
            mcp_status: crate::notice::UiNotice::SubagentMcpDisabled,
            profiles: Default::default(),
            records,
        }));
        let (ui, _receiver) = watch::channel(UiSnapshot::default());
        Reporter {
            id,
            state,
            ui,
            persistence: Arc::new(Mutex::new(None)),
            auto_approve_shell: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn terminal_statuses_are_never_counted_as_active() {
        assert!(SubagentStatus::Running.is_active());
        assert!(SubagentStatus::WaitingDependencies.is_active());
        assert!(SubagentStatus::WaitingApproval.is_active());
        assert!(SubagentStatus::WaitingBudget.is_active());
        assert!(!SubagentStatus::RecoveryRequired.is_active());
        assert!(!SubagentStatus::RecoveryRequired.is_terminal());
        assert!(SubagentStatus::RecoveryRequired.is_recoverable());
        assert!(!SubagentStatus::ReadyForReview.is_active());
        assert!(!SubagentStatus::Completed.is_active());
        assert!(!SubagentStatus::Failed.is_active());
        assert!(SubagentStatus::DependencyFailed.is_terminal());
    }

    #[test]
    fn research_agents_only_receive_explicitly_safe_mcp_tools() {
        assert!(super::subagent_mcp_tool_allowed(
            SubagentMode::Research,
            Some(true),
            Some(false),
            Some(false),
        ));
        assert!(!super::subagent_mcp_tool_allowed(
            SubagentMode::Research,
            None,
            None,
            None,
        ));
        assert!(!super::subagent_mcp_tool_allowed(
            SubagentMode::Research,
            Some(true),
            Some(true),
            Some(false),
        ));
        assert!(super::subagent_mcp_tool_allowed(
            SubagentMode::Writer,
            None,
            Some(true),
            Some(true),
        ));
    }

    #[test]
    fn recursive_tool_surface_removes_spawn_exactly_at_depth_limit() -> Result<(), &'static str> {
        let profiles = vec!["builtin:research".to_owned(), "builtin:writer".to_owned()];
        let enabled = super::recursive_function_definitions(&profiles, true);
        let spawn = enabled
            .iter()
            .find(|tool| tool.name == "spawn_agent")
            .ok_or("spawn tool should be advertised with available profiles")?;
        assert!(
            spawn.parameters["properties"]["depends_on"]
                .get("uniqueItems")
                .is_none()
        );
        assert!(
            spawn.parameters["properties"]["file_claims"]
                .get("uniqueItems")
                .is_none()
        );
        assert!(enabled.iter().any(|tool| tool.name == "wait_agent"));
        let bounded = super::recursive_function_definitions(&profiles, false);
        assert!(!bounded.iter().any(|tool| tool.name == "spawn_agent"));
        assert!(bounded.iter().any(|tool| tool.name == "list_agents"));

        let empty = super::recursive_function_definitions(&[], true);
        assert!(!empty.iter().any(|tool| tool.name == "spawn_agent"));
        Ok(())
    }

    #[test]
    fn persisted_recursive_tree_rejects_cycles_and_invalid_depth() {
        let parent = PersistedAgent::from_snapshot(&snapshot(), None);
        let mut child_snapshot = snapshot();
        child_snapshot.id = SubagentId::new(2);
        child_snapshot.parent_id = Some(parent.id);
        child_snapshot.depth = 2;
        let child = PersistedAgent::from_snapshot(&child_snapshot, None);
        let mut corrupt_child = child.clone();
        let mut records = BTreeMap::from([(parent.id, parent), (child.id, child)]);
        assert!(super::validate_persisted_parent_tree(&records, 3, 4).is_ok());

        corrupt_child.parent_id = Some(corrupt_child.id);
        records.insert(corrupt_child.id, corrupt_child);
        assert!(super::validate_persisted_parent_tree(&records, 3, 4).is_err());
    }

    #[tokio::test]
    async fn mcp_approval_never_inherits_shell_auto_approval()
    -> Result<(), Box<dyn std::error::Error>> {
        let agent = snapshot();
        let reporter = schedule_reporter(agent.id, vec![(agent, false)]);
        reporter
            .auto_approve_shell
            .store(true, std::sync::atomic::Ordering::Release);
        let task_reporter = reporter.clone();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async move {
            task_reporter
                .request_mcp_approval(
                    17,
                    "files::read".to_owned(),
                    &serde_json::json!({"path": "src/lib.rs"}),
                    &cancel,
                )
                .await
        });

        let mut observed = false;
        for _ in 0..100 {
            observed = reporter
                .state
                .lock()
                .map_err(|_| "state lock poisoned")?
                .records
                .get(&reporter.id)
                .and_then(|record| record.snapshot.pending_command.as_ref())
                .is_some_and(|pending| pending.action_id == 17 && pending.mcp);
            if observed {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(observed, "MCP call must pause for an explicit decision");

        let approval = reporter
            .state
            .lock()
            .map_err(|_| "state lock poisoned")?
            .records
            .get_mut(&reporter.id)
            .and_then(|record| record.approval.take())
            .ok_or("approval sender missing")?;
        approval
            .send(true)
            .map_err(|_| "approval receiver closed")?;
        assert!(task.await??);
        let pending_cleared = reporter
            .state
            .lock()
            .map_err(|_| "state lock poisoned")?
            .records
            .get(&reporter.id)
            .is_some_and(|record| record.snapshot.pending_command.is_none());
        assert!(pending_cleared);
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_shell_approval_stops_the_worker() -> Result<(), Box<dyn std::error::Error>>
    {
        let value = snapshot();
        let reporter = schedule_reporter(value.id, vec![(value, false)]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = reporter
            .request_command_approval(7, "cargo test".to_owned(), false, &cancel)
            .await;

        assert!(matches!(
            result,
            Err(SubagentError::Api(crate::error::ApiError::Cancelled))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_mcp_approval_stops_the_worker() -> Result<(), Box<dyn std::error::Error>> {
        let value = snapshot();
        let reporter = schedule_reporter(value.id, vec![(value, false)]);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = reporter
            .request_mcp_approval(
                7,
                "files::write".to_owned(),
                &serde_json::json!({}),
                &cancel,
            )
            .await;

        assert!(matches!(
            result,
            Err(SubagentError::Api(crate::error::ApiError::Cancelled))
        ));
        Ok(())
    }

    #[test]
    fn authoritative_scheduler_waits_then_atomically_reserves_a_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut predecessor = snapshot();
        predecessor.status = SubagentStatus::Running;
        let mut dependent = snapshot();
        dependent.id = SubagentId::new(2);
        dependent.mode = SubagentMode::Writer;
        dependent.status = SubagentStatus::WaitingDependencies;
        dependent.dependencies = Arc::from([predecessor.id]);
        dependent.file_claims = Arc::from(["src/parser.rs".to_owned()]);
        let reporter =
            schedule_reporter(dependent.id, vec![(predecessor, false), (dependent, false)]);

        assert!(matches!(
            reporter.try_reserve_schedule()?,
            ScheduleReservation::Waiting(message) if message.contains("agent-0001")
        ));
        {
            let mut state = reporter.state.lock().map_err(|_| "state lock poisoned")?;
            let record = state
                .records
                .get_mut(&SubagentId::new(1))
                .ok_or("predecessor missing")?;
            record.snapshot.status = SubagentStatus::Completed;
        }
        assert_eq!(reporter.try_reserve_schedule()?, ScheduleReservation::Ready);
        let reserved = reporter
            .state
            .lock()
            .map_err(|_| "state lock poisoned")?
            .records
            .get(&SubagentId::new(2))
            .is_some_and(|record| record.schedule_reserved);
        assert!(reserved);
        Ok(())
    }

    #[tokio::test]
    async fn completed_agents_still_count_toward_the_session_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        let (api, mut agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            "http://127.0.0.1:9/responses".to_owned(),
        );
        agent.subagents.max_per_session = 1;
        let (ui, _snapshots) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        let mut completed = snapshot();
        completed.status = SubagentStatus::Completed;
        completed.session_id = Some("session-a".to_owned());
        let (messages, receiver) = mpsc::channel(1);
        drop(receiver);
        coordinator
            .state
            .lock()
            .map_err(|_| "state lock poisoned")?
            .records
            .insert(
                completed.id,
                AgentRecord {
                    snapshot: completed,
                    cancel: CancellationToken::new(),
                    messages,
                    approval: None,
                    budget_approval: None,
                    restart_recovery: Arc::new(AtomicBool::new(false)),
                    worktree: None,
                    changes: None,
                    recovery: None,
                    schedule_reserved: false,
                    reserved_tokens: 0,
                },
            );

        let result = coordinator
            .spawn(SpawnSubagentRequest {
                task: "second task".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("session-a".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test".to_owned(),
                dependencies: Vec::new(),
                file_claims: Vec::new(),
            })
            .await;

        assert!(matches!(
            result,
            Err(SubagentError::SessionLimit { limit: 1 })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn failed_mcp_enable_does_not_leave_the_ui_starting()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        let (api, mut agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            "http://127.0.0.1:9/responses".to_owned(),
        );
        agent.subagents.allow_mcp = true;
        let mut mcp = crate::mcp::McpConfig {
            enabled: true,
            ..crate::mcp::McpConfig::default()
        };
        mcp.startup_timeout = Duration::ZERO;
        let (ui, _snapshots) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new_with_mcp(api, agent, ui, Some(mcp))?;
        coordinator.set_mcp_enabled(false).await?;

        assert!(coordinator.set_mcp_enabled(true).await.is_err());
        let fleet = coordinator.snapshot();
        assert!(!fleet.mcp_enabled);
        assert!(matches!(
            fleet.mcp_status,
            crate::notice::UiNotice::ExternalError { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_parent_cannot_spawn_more_children() -> Result<(), Box<dyn std::error::Error>>
    {
        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        let (api, agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            "http://127.0.0.1:9/responses".to_owned(),
        );
        let (ui, _snapshots) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        let mut parent = snapshot();
        parent.status = SubagentStatus::Cancelling;
        parent.session_id = Some("session-a".to_owned());
        let parent_id = parent.id;
        let (messages, receiver) = mpsc::channel(1);
        drop(receiver);
        {
            let mut state = coordinator
                .state
                .lock()
                .map_err(|_| "state lock poisoned")?;
            state.next_id = 2;
            state.records.insert(
                parent_id,
                AgentRecord {
                    snapshot: parent,
                    cancel: CancellationToken::new(),
                    messages,
                    approval: None,
                    budget_approval: None,
                    restart_recovery: Arc::new(AtomicBool::new(false)),
                    worktree: None,
                    changes: None,
                    recovery: None,
                    schedule_reserved: false,
                    reserved_tokens: 0,
                },
            );
        }

        let result = coordinator
            .spawn_at(
                SpawnSubagentRequest {
                    task: "late child".to_owned(),
                    profile_id: "builtin:research".to_owned(),
                    session_id: Some("session-a".to_owned()),
                    deployment: "test-model".to_owned(),
                    reasoning_effort: ReasoningEffort::High,
                    instructions: "test".to_owned(),
                    dependencies: Vec::new(),
                    file_claims: Vec::new(),
                },
                Some(parent_id),
                1,
                Some(workspace.path().to_path_buf()),
            )
            .await;

        assert!(matches!(result, Err(SubagentError::NotRunning { id }) if id == parent_id));
        Ok(())
    }

    #[test]
    fn scheduler_propagates_dependency_failure_without_starting_the_child()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut predecessor = snapshot();
        predecessor.status = SubagentStatus::Failed;
        let mut dependent = snapshot();
        dependent.id = SubagentId::new(2);
        dependent.dependencies = Arc::from([predecessor.id]);
        let reporter =
            schedule_reporter(dependent.id, vec![(predecessor, false), (dependent, false)]);

        assert_eq!(
            reporter.try_reserve_schedule()?,
            ScheduleReservation::Failed(vec![SubagentId::new(1)])
        );
        Ok(())
    }

    #[test]
    fn writer_file_claims_serialize_only_overlapping_work() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut holder = snapshot();
        holder.mode = SubagentMode::Writer;
        holder.file_claims = Arc::from(["src".to_owned()]);
        let mut blocked = snapshot();
        blocked.id = SubagentId::new(2);
        blocked.mode = SubagentMode::Writer;
        blocked.file_claims = Arc::from(["src/parser.rs".to_owned()]);
        let reporter = schedule_reporter(holder.id, vec![(holder, true), (blocked, false)]);
        let blocked_reporter = Reporter {
            id: SubagentId::new(2),
            ..reporter.clone()
        };

        assert!(matches!(
            blocked_reporter.try_reserve_schedule()?,
            ScheduleReservation::Waiting(message) if message.contains("agent-0001")
        ));
        {
            let mut state = reporter.state.lock().map_err(|_| "state lock poisoned")?;
            let record = state
                .records
                .get_mut(&SubagentId::new(1))
                .ok_or("writer holder missing")?;
            record.schedule_reserved = false;
        }
        assert_eq!(
            blocked_reporter.try_reserve_schedule()?,
            ScheduleReservation::Ready
        );
        Ok(())
    }

    #[test]
    fn writer_review_is_a_hard_dependency_until_the_user_resolves_it() {
        let mut writer = snapshot();
        writer.mode = SubagentMode::Writer;
        writer.status = SubagentStatus::ReadyForReview;
        writer.changed_files = Arc::from(["src/lib.rs".to_owned()]);
        assert_eq!(dependency_state(&writer), super::DependencyState::Pending);

        writer.status = SubagentStatus::Failed;
        assert_eq!(dependency_state(&writer), super::DependencyState::Pending);
        writer.changed_files = Arc::from([]);
        assert_eq!(dependency_state(&writer), super::DependencyState::Failed);
        writer.status = SubagentStatus::Completed;
        assert_eq!(dependency_state(&writer), super::DependencyState::Succeeded);
    }

    #[test]
    fn token_reservations_are_atomic_and_release_capacity_on_drop()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut first = snapshot();
        first.session_id = Some("budget-session".to_owned());
        first.total_tokens = 400;
        first.token_budget = 1_500;
        let mut second = snapshot();
        second.id = SubagentId::new(2);
        second.session_id = first.session_id.clone();
        second.total_tokens = 500;
        second.token_budget = 1_500;
        let first_reporter = schedule_reporter(first.id, vec![(first, false), (second, false)]);
        {
            let mut state = first_reporter
                .state
                .lock()
                .map_err(|_| "state lock poisoned")?;
            state.max_tokens_per_agent = 1_500;
            state.max_total_tokens_per_session = 2_000;
        }
        let second_reporter = Reporter {
            id: SubagentId::new(2),
            ..first_reporter.clone()
        };

        let first_reservation = first_reporter.reserve_request_budget(400, 900)?;
        assert_eq!(first_reservation.granted_output(), 700);
        assert!(matches!(
            second_reporter.reserve_request_budget(100, 500),
            Err(SubagentError::TokenBudgetExhausted {
                scope: SubagentBudgetScope::SessionTree,
                used: 2_000,
                limit: 2_000
            })
        ));

        drop(first_reservation);
        let second_reservation = second_reporter.reserve_request_budget(100, 500)?;
        assert_eq!(second_reservation.granted_output(), 500);
        second_reservation.commit(120, 80, 200)?;
        let state = second_reporter
            .state
            .lock()
            .map_err(|_| "state lock poisoned")?;
        let second = state
            .records
            .get(&SubagentId::new(2))
            .ok_or("second agent missing")?;
        assert_eq!(second.reserved_tokens, 0);
        assert_eq!(second.snapshot.total_tokens, 700);
        Ok(())
    }

    #[tokio::test]
    async fn budget_guard_waits_for_an_explicit_raise_and_persists_the_new_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = snapshot();
        let reporter = schedule_reporter(value.id, vec![(value, false)]);
        let waiting = reporter.clone();
        let cancel = CancellationToken::new();
        let waiter = tokio::spawn(async move {
            waiting
                .request_budget_increase(SubagentBudgetScope::Agent, 150_000, 150_000, &cancel)
                .await
        });
        let mut sender = None;
        for _ in 0..32 {
            {
                let mut state = reporter.state.lock().map_err(|_| "state lock poisoned")?;
                let record = state.records.get_mut(&reporter.id).ok_or("agent missing")?;
                if record.snapshot.status == SubagentStatus::WaitingBudget {
                    assert_eq!(
                        record
                            .snapshot
                            .pending_budget
                            .as_ref()
                            .map(|pending| pending.scope),
                        Some(SubagentBudgetScope::Agent)
                    );
                    sender = record.budget_approval.take();
                }
            }
            if sender.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let sender = sender.ok_or("budget decision sender missing")?;
        sender.send(true).map_err(|_| "budget waiter closed")?;
        assert!(waiter.await??);

        let state = reporter.state.lock().map_err(|_| "state lock poisoned")?;
        let record = state.records.get(&reporter.id).ok_or("agent missing")?;
        assert_eq!(record.snapshot.status, SubagentStatus::Running);
        assert_eq!(record.snapshot.token_budget, 200_000);
        assert!(record.snapshot.pending_budget.is_none());
        assert!(record.budget_approval.is_none());
        assert_eq!(
            state.session_token_budget(&record.snapshot.session_id),
            550_000
        );
        Ok(())
    }

    #[test]
    fn ready_for_review_descendant_keeps_parent_open() {
        let mut child = snapshot();
        child.status = SubagentStatus::ReadyForReview;
        child.changed_files = Arc::from(["src/lib.rs".to_owned()]);
        assert!(descendant_requires_resolution(&child));

        child.status = SubagentStatus::Completed;
        child.changed_files = Arc::from([]);
        assert!(!descendant_requires_resolution(&child));
    }

    #[test]
    fn nested_writer_does_not_deadlock_on_ancestor_file_claim()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut parent = snapshot();
        parent.mode = SubagentMode::Writer;
        parent.file_claims = Arc::from(["src".to_owned()]);
        let mut child = snapshot();
        child.id = SubagentId::new(2);
        child.parent_id = Some(parent.id);
        child.depth = 2;
        child.mode = SubagentMode::Writer;
        child.file_claims = Arc::from(["src/parser.rs".to_owned()]);
        let reporter = schedule_reporter(parent.id, vec![(parent, true), (child, false)]);
        let child_reporter = Reporter {
            id: SubagentId::new(2),
            ..reporter
        };

        assert_eq!(
            child_reporter.try_reserve_schedule()?,
            ScheduleReservation::Ready
        );
        Ok(())
    }

    #[test]
    fn only_nonterminal_writer_states_are_restored_as_recoverable() {
        assert!(should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::Running
        ));
        assert!(should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::WaitingDependencies
        ));
        assert!(should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::RecoveryRequired
        ));
        assert!(should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::Interrupted
        ));
        assert!(!should_restore_writer(
            SubagentMode::Research,
            SubagentStatus::Running
        ));
        assert!(!should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::Completed
        ));
        assert!(!should_restore_writer(
            SubagentMode::Writer,
            SubagentStatus::Cancelled
        ));
    }

    #[test]
    fn fresh_worktree_resume_is_allowed_only_before_any_uncertain_action()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut fresh = super::initial_recovery_state("queued writer", true)?;
        assert!(fresh.summary(false).can_resume);
        fresh.pending_action = Some(SubagentRecoveryAction {
            action_id: 1,
            action: ToolAction::WriteFile {
                path: "file.txt".to_owned(),
                content: "unknown".to_owned(),
            },
        });
        assert!(!fresh.summary(false).can_resume);
        assert!(fresh.summary(true).can_resume);

        let legacy_active = super::initial_recovery_state("legacy writer", false)?;
        assert!(!legacy_active.summary(false).can_resume);
        Ok(())
    }

    #[test]
    fn persisted_recovery_round_trip_keeps_transcript_and_wal_action()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut value = snapshot();
        value.mode = SubagentMode::Writer;
        value.parent_id = Some(SubagentId::new(8));
        value.depth = 2;
        value.status = SubagentStatus::RecoveryRequired;
        value.dependencies = Arc::from([SubagentId::new(9)]);
        value.file_claims = Arc::from(["src/lib.rs".to_owned()]);
        push_transcript(&mut value, "tool".to_owned(), "command was announced");
        let recovery = recovery_state(Some(SubagentRecoveryAction {
            action_id: 7,
            action: ToolAction::ExecuteCommand {
                command: "cargo check".to_owned(),
                requires_confirmation: false,
            },
        }))?;
        let encoded = serde_json::to_vec(&PersistedAgent::from_snapshot(
            &value,
            Some(recovery.clone()),
        ))?;
        let decoded: PersistedAgent = serde_json::from_slice(&encoded)?;
        assert_eq!(decoded.recovery.as_ref(), Some(&recovery));
        assert_eq!(decoded.transcript, value.transcript.as_ref());
        assert_eq!(decoded.dependencies, value.dependencies.as_ref());
        assert_eq!(decoded.file_claims, value.file_claims.as_ref());
        assert_eq!(decoded.parent_id, value.parent_id);
        assert_eq!(decoded.depth, value.depth);
        assert_eq!(decoded.token_budget, value.token_budget);

        let restored = decoded.into_snapshot();
        assert_eq!(restored.transcript, value.transcript);
        assert_eq!(restored.dependencies, value.dependencies);
        assert_eq!(restored.file_claims, value.file_claims);
        assert_eq!(restored.parent_id, value.parent_id);
        assert_eq!(restored.depth, value.depth);
        assert_eq!(restored.token_budget, value.token_budget);
        let summary = restored
            .recovery
            .ok_or("recovery summary was not restored")?;
        assert_eq!(summary.attempt, 2);
        assert_eq!(summary.uncertain_action, recovery.pending_action);
        assert!(!summary.can_resume);
        Ok(())
    }

    #[test]
    fn legacy_journal_without_recovery_fields_remains_readable()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut encoded = serde_json::to_value(PersistedAgent::from_snapshot(&snapshot(), None))?;
        let object = encoded
            .as_object_mut()
            .ok_or("persisted agent did not serialize to an object")?;
        object.remove("transcript");
        object.remove("recovery");
        object.remove("dependencies");
        object.remove("file_claims");
        object.remove("parent_id");
        object.remove("depth");
        let decoded: PersistedAgent = serde_json::from_value(encoded)?;
        assert!(decoded.transcript.is_empty());
        assert!(decoded.recovery.is_none());
        assert!(decoded.dependencies.is_empty());
        assert!(decoded.file_claims.is_empty());
        assert_eq!(decoded.parent_id, None);
        assert_eq!(decoded.depth, 1);
        Ok(())
    }

    #[test]
    fn uncertain_action_becomes_unknown_result_instead_of_a_blind_replay()
    -> Result<(), Box<dyn std::error::Error>> {
        let uncertain = SubagentRecoveryAction {
            action_id: 9,
            action: ToolAction::ExecuteCommand {
                command: "dangerous-side-effect".to_owned(),
                requires_confirmation: true,
            },
        };
        let recovery = recovery_state(Some(uncertain))?;
        let (replay, next_action_id, attempt, dependency_context_added) =
            prepare_recovery_replay("original task", Some(recovery))?;
        let encoded = serde_json::to_string(&replay)?;
        assert!(encoded.contains("interrupted_unknown"));
        assert!(encoded.contains("Never blindly repeat"));
        assert!(encoded.contains("inspect the existing isolated worktree"));
        assert_eq!(next_action_id, 10);
        assert_eq!(attempt, 2);
        assert!(!dependency_context_added);
        Ok(())
    }

    #[test]
    fn oversized_recovery_state_fails_before_journal_write()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut recovery = recovery_state(None)?;
        recovery.replay = vec![serde_json::Value::String(
            "x".repeat(MAX_RECOVERY_STATE_BYTES.saturating_add(1)),
        )];
        assert!(matches!(
            validate_recovery_state(&recovery),
            Err(super::SubagentError::RecoveryStateTooLarge { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn oversized_recovery_state_is_rejected_when_loading_the_journal()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("subagents.jsonl");
        let mut recovery = recovery_state(None)?;
        recovery.replay = vec![serde_json::Value::String(
            "x".repeat(MAX_RECOVERY_STATE_BYTES.saturating_add(1)),
        )];
        let mut encoded =
            serde_json::to_vec(&PersistedAgent::from_snapshot(&snapshot(), Some(recovery)))?;
        encoded.push(b'\n');
        fs::write(&path, encoded)?;

        assert!(matches!(
            load_journal(&path).await,
            Err(SubagentError::RecoveryStateTooLarge { .. })
        ));
        Ok(())
    }

    #[test]
    fn transcript_is_bounded_and_keeps_newest_entries() {
        let mut value = snapshot();
        for index in 0..200 {
            push_transcript(&mut value, "step".to_owned(), &format!("entry {index}"));
        }
        assert_eq!(value.transcript.len(), 128);
        assert_eq!(value.transcript[0].content, "entry 72");
        assert_eq!(value.transcript[127].content, "entry 199");
    }

    #[test]
    fn display_text_bounds_preserve_utf8() {
        let text = "Ж".repeat(100);
        let bounded = bounded_text(&text, 37);
        assert!(bounded.is_char_boundary(bounded.len()));
        assert!(bounded.ends_with("[truncated]"));
        assert_eq!(compact_line(" one\n two   three ", 20), "one two three");
    }

    #[tokio::test]
    async fn journal_uses_latest_valid_revision_and_ignores_torn_tail()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("subagents.jsonl");
        let first = PersistedAgent::from_snapshot(&snapshot(), None);
        let mut second_snapshot = snapshot();
        second_snapshot.revision = 2;
        second_snapshot.status = SubagentStatus::Completed;
        second_snapshot.result = "durable result".to_owned();
        let second = PersistedAgent::from_snapshot(&second_snapshot, None);
        let mut bytes = serde_json::to_vec(&first)?;
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&second)?);
        bytes.push(b'\n');
        bytes.extend(br#"{"id":1,"revision"#);
        fs::write(&path, bytes)?;

        let loaded = load_journal(&path).await?;
        let Some(agent) = loaded.get(&SubagentId::new(1)) else {
            return Err("valid sub-agent journal records were not restored".into());
        };
        assert_eq!(agent.revision, 2);
        assert_eq!(agent.status, SubagentStatus::Completed);
        assert_eq!(agent.result, "durable result");
        Ok(())
    }

    #[tokio::test]
    async fn durable_ack_is_sent_only_after_wal_record_can_be_reloaded()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("subagents.jsonl");
        let mut value = snapshot();
        value.mode = SubagentMode::Writer;
        value.status = SubagentStatus::Running;
        let recovery = recovery_state(Some(SubagentRecoveryAction {
            action_id: 3,
            action: ToolAction::WriteFile {
                path: "src/recovered.rs".to_owned(),
                content: "pending".to_owned(),
            },
        }))?;
        let record = PersistedAgent::from_snapshot(&value, Some(recovery.clone()));
        let (updates, receiver) = mpsc::unbounded_channel();
        let writer_path = path.clone();
        let writer =
            tokio::spawn(
                async move { journal_writer(writer_path, BTreeMap::new(), receiver).await },
            );
        let (acknowledgement, acknowledged) = oneshot::channel();
        updates.send(PersistenceUpdate {
            record,
            acknowledgement: Some(acknowledgement),
        })?;
        match acknowledged.await? {
            Ok(()) => {}
            Err(message) => return Err(message.into()),
        }

        let loaded = load_journal(&path).await?;
        let persisted = loaded
            .get(&value.id)
            .ok_or("durably acknowledged record was not reloadable")?;
        assert_eq!(persisted.recovery.as_ref(), Some(&recovery));
        drop(updates);
        writer.await??;
        Ok(())
    }

    #[tokio::test]
    async fn completed_dependency_handoff_reaches_the_child_as_responses_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(150))
                    .set_body_raw(completed_sse(), "text/event-stream"),
            )
            .expect(2)
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        initialize_git(workspace.path())?;
        fs::write(workspace.path().join("base.txt"), "base\n")?;
        let (api, agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            format!("{}/responses", server.uri()),
        );
        let (ui, _ui_rx) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        coordinator.start().await;

        let predecessor = coordinator
            .spawn(SpawnSubagentRequest {
                task: "inspect the parser contract".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("dag-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test instructions".to_owned(),
                dependencies: Vec::new(),
                file_claims: Vec::new(),
            })
            .await?;
        let dependent = coordinator
            .spawn(SpawnSubagentRequest {
                task: "verify the dependent parser behavior".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("dag-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test instructions".to_owned(),
                dependencies: vec![predecessor],
                file_claims: Vec::new(),
            })
            .await?;
        assert_eq!(
            coordinator.agent_snapshot(dependent)?.status,
            SubagentStatus::WaitingDependencies
        );
        let predecessor_snapshot = wait_until_settled(&coordinator, predecessor).await?;
        assert_eq!(predecessor_snapshot.status, SubagentStatus::Completed);
        let dependent_snapshot = wait_until_settled(&coordinator, dependent).await?;
        coordinator.shutdown().await;
        assert_eq!(dependent_snapshot.status, SubagentStatus::Completed);
        assert_eq!(dependent_snapshot.dependencies.as_ref(), [predecessor]);

        let requests = server
            .received_requests()
            .await
            .ok_or("wiremock did not retain received requests")?;
        assert_eq!(requests.len(), 2);
        let body: serde_json::Value = serde_json::from_slice(&requests[1].body)?;
        assert!(body.get("input").is_some());
        assert!(body.get("messages").is_none());
        let body_text = serde_json::to_string(&body)?;
        assert!(body_text.contains("DEPENDENCY HANDOFFS"));
        assert!(body_text.contains("Recovered safely"));
        assert!(body_text.contains("agent-0001"));
        Ok(())
    }

    #[tokio::test]
    async fn recursive_child_runs_with_one_parallel_slot_and_is_tree_scoped()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                let sse = if body.contains("function_call_output")
                    || body.contains("child recursive inspection")
                {
                    completed_sse().to_owned()
                } else {
                    recursive_spawn_sse()
                };
                ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
            })
            .expect(4)
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        initialize_git(workspace.path())?;
        fs::write(workspace.path().join("base.txt"), "base\n")?;
        let (api, mut agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            format!("{}/responses", server.uri()),
        );
        agent.subagents.max_parallel = 1;
        agent.subagents.max_depth = 3;
        let (ui, _ui_rx) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        coordinator.start().await;
        let parent = coordinator
            .spawn(SpawnSubagentRequest {
                task: "delegate one recursive inspection and integrate it".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("recursive-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test recursive instructions".to_owned(),
                dependencies: Vec::new(),
                file_claims: Vec::new(),
            })
            .await?;
        let parent_snapshot = wait_until_settled(&coordinator, parent).await?;
        let mut descendants = coordinator.descendants(parent)?;
        if let Some(child) = descendants.first() {
            let settled = wait_until_settled(&coordinator, child.id).await?;
            descendants[0] = settled;
        }
        coordinator.shutdown().await;

        assert_eq!(parent_snapshot.status, SubagentStatus::Completed);
        assert_eq!(parent_snapshot.depth, 1);
        assert_eq!(descendants.len(), 1);
        assert_eq!(descendants[0].parent_id, Some(parent));
        assert_eq!(descendants[0].depth, 2);
        assert_eq!(descendants[0].status, SubagentStatus::Completed);
        assert!(matches!(
            coordinator.ensure_descendant(descendants[0].id, parent),
            Err(super::SubagentError::DescendantAccess { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn failed_dependency_stops_child_without_an_api_request()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        initialize_git(workspace.path())?;
        let (api, agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            format!("{}/responses", server.uri()),
        );
        let (ui, _ui_rx) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        coordinator.start().await;

        let predecessor = coordinator
            .spawn(SpawnSubagentRequest {
                task: "perform failing research".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("failed-dag-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test instructions".to_owned(),
                dependencies: Vec::new(),
                file_claims: Vec::new(),
            })
            .await?;
        assert_eq!(
            wait_until_settled(&coordinator, predecessor).await?.status,
            SubagentStatus::Failed
        );

        let dependent = coordinator
            .spawn(SpawnSubagentRequest {
                task: "must not run after a failed prerequisite".to_owned(),
                profile_id: "builtin:research".to_owned(),
                session_id: Some("failed-dag-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test instructions".to_owned(),
                dependencies: vec![predecessor],
                file_claims: Vec::new(),
            })
            .await?;
        let dependent = wait_until_settled(&coordinator, dependent).await?;
        coordinator.shutdown().await;
        assert_eq!(dependent.status, SubagentStatus::DependencyFailed);
        assert!(
            dependent
                .error
                .as_deref()
                .is_some_and(|error| error.contains("agent-0001"))
        );
        Ok(())
    }

    #[tokio::test]
    async fn writer_changes_outside_declared_claims_fail_closed_in_the_worktree()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(|request: &wiremock::Request| {
                let body = String::from_utf8_lossy(&request.body);
                let sse = if body.contains("tool_result") {
                    completed_sse()
                } else {
                    writer_outside_claim_sse()
                };
                ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
            })
            .expect(2)
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        initialize_git(workspace.path())?;
        fs::write(workspace.path().join("base.txt"), "base\n")?;
        let (api, agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            format!("{}/responses", server.uri()),
        );
        let (ui, _ui_rx) = watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        coordinator.start().await;

        let writer = coordinator
            .spawn(SpawnSubagentRequest {
                task: "write outside the declared claim".to_owned(),
                profile_id: "builtin:writer".to_owned(),
                session_id: Some("claim-session".to_owned()),
                deployment: "test-model".to_owned(),
                reasoning_effort: ReasoningEffort::High,
                instructions: "test instructions".to_owned(),
                dependencies: Vec::new(),
                file_claims: vec!["src/parser.rs".to_owned()],
            })
            .await?;
        let writer = wait_until_settled(&coordinator, writer).await?;
        coordinator.shutdown().await;

        assert_eq!(writer.status, SubagentStatus::RecoveryRequired);
        assert_eq!(writer.changed_files.as_ref(), ["outside.rs"]);
        assert!(
            writer
                .error
                .as_deref()
                .is_some_and(|error| error.contains("outside its declared file claims"))
        );
        assert!(!workspace.path().join("outside.rs").exists());
        Ok(())
    }

    #[tokio::test]
    async fn restarted_writer_resumes_same_worktree_with_responses_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(completed_sse(), "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;

        let workspace = tempfile::tempdir()?;
        let managed = tempfile::tempdir()?;
        initialize_git(workspace.path())?;
        fs::write(workspace.path().join("base.txt"), "base\n")?;
        let manager = crate::agent::worktree::WorktreeManager::open(
            workspace.path(),
            managed.path(),
            Duration::from_secs(10),
        )
        .await?;
        let id = SubagentId::new(17);
        let worktree = manager.create(id.get()).await?;
        fs::write(worktree.path.join("agent.txt"), "already applied\n")?;

        let mut value = snapshot();
        value.id = id;
        value.revision = 8;
        value.mode = SubagentMode::Writer;
        value.profile_id = "builtin:writer".to_owned();
        value.profile_name = "Writer".to_owned();
        value.status = SubagentStatus::Running;
        value.worktree = Some(worktree.path.display().to_string());
        value.base_commit = Some(worktree.base_commit.clone());
        let recovery = RecoveryState {
            replay: vec![message_value(InputMessage::user(
                "finish recovered writer",
            ))?],
            next_action_id: 4,
            attempt: 0,
            checkpoint_at: Utc::now(),
            pending_action: Some(SubagentRecoveryAction {
                action_id: 3,
                action: ToolAction::WriteFile {
                    path: "agent.txt".to_owned(),
                    content: "already applied\n".to_owned(),
                },
            }),
            reason: "writer interrupted after WAL intent".to_owned(),
            allow_fresh_worktree: false,
            dependency_context_added: false,
        };
        let journal = manager.control_root().join("subagents.jsonl");
        let mut encoded =
            serde_json::to_vec(&PersistedAgent::from_snapshot(&value, Some(recovery)))?;
        encoded.push(b'\n');
        fs::write(&journal, encoded)?;
        drop(manager);

        let (api, agent) = coordinator_configs(
            workspace.path(),
            managed.path(),
            format!("{}/responses", server.uri()),
        );
        let (ui, _ui_rx) = tokio::sync::watch::channel(UiSnapshot::default());
        let coordinator = SubagentCoordinator::new(api, agent, ui)?;
        coordinator.start().await;
        let restored = coordinator.agent_snapshot(id)?;
        assert_eq!(restored.status, SubagentStatus::RecoveryRequired);
        assert_eq!(restored.worktree.as_deref(), worktree_path(&worktree.path));
        assert!(
            restored
                .changed_files
                .iter()
                .any(|path| path == "agent.txt")
        );
        assert!(
            restored
                .recovery
                .as_ref()
                .is_some_and(|summary| summary.can_resume)
        );

        coordinator
            .resume(id, restored.revision, "test instructions".to_owned())
            .await?;
        let mut current = coordinator.agent_snapshot(id)?;
        for _ in 0..32 {
            if !current.status.is_active() {
                break;
            }
            current = coordinator
                .wait_for_update(
                    id,
                    current.revision,
                    Duration::from_secs(2),
                    &CancellationToken::new(),
                )
                .await?;
        }
        coordinator.shutdown().await;

        assert_eq!(current.status, SubagentStatus::ReadyForReview);
        assert_eq!(current.result, "Recovered safely");
        assert!(current.recovery.is_none());
        assert!(current.changed_files.iter().any(|path| path == "agent.txt"));
        assert_eq!(
            fs::read_to_string(worktree.path.join("agent.txt"))?,
            "already applied\n"
        );
        let requests = server
            .received_requests()
            .await
            .ok_or("wiremock did not retain received requests")?;
        let request = requests
            .first()
            .ok_or("writer recovery did not call the Responses API")?;
        let body: serde_json::Value = serde_json::from_slice(&request.body)?;
        assert!(body.get("input").is_some());
        assert!(body.get("messages").is_none());
        assert_eq!(body["stream"], true);
        let body_text = serde_json::to_string(&body)?;
        assert!(body_text.contains("interrupted_unknown"));
        assert!(body_text.contains("inspect the existing isolated worktree"));
        Ok(())
    }

    fn recovery_state(
        pending_action: Option<SubagentRecoveryAction>,
    ) -> Result<RecoveryState, Box<dyn std::error::Error>> {
        Ok(RecoveryState {
            replay: vec![message_value(InputMessage::user("original task"))?],
            next_action_id: 4,
            attempt: 2,
            checkpoint_at: Utc::now(),
            pending_action,
            reason: "test checkpoint".to_owned(),
            allow_fresh_worktree: false,
            dependency_context_added: false,
        })
    }

    fn initialize_git(workspace: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace)
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        Ok(())
    }

    async fn wait_until_settled(
        coordinator: &SubagentCoordinator,
        id: SubagentId,
    ) -> Result<SubagentSnapshot, Box<dyn std::error::Error>> {
        let cancel = CancellationToken::new();
        let mut current = coordinator.agent_snapshot(id)?;
        for _ in 0..32 {
            if !current.status.is_active() {
                return Ok(current);
            }
            current = coordinator
                .wait_for_update(id, current.revision, Duration::from_secs(2), &cancel)
                .await?;
        }
        Err(format!("{id} did not settle after 32 authoritative updates").into())
    }

    fn coordinator_configs(
        workspace: &Path,
        managed: &Path,
        endpoint: String,
    ) -> (ApiConfig, AgentConfig) {
        (
            ApiConfig {
                provider: crate::config::ApiProvider::Azure,
                auth: crate::config::ApiAuth::ApiKey,
                api_key: SecretString::new("test-key".to_owned().into()),
                bedrock_runtime: crate::config::BedrockRuntimeConfig::default(),
                transport: crate::config::ApiTransport::Sse,
                endpoint: ResponsesEndpoint::FullUrl(endpoint),
                allow_insecure_loopback: true,
                deployment: "test-model".to_owned(),
                deployment_choices: vec!["test-model".to_owned()],
                api_version: None,
                max_output_tokens: 512,
                reasoning_effort: ReasoningEffort::High,
                temperature: None,
                server_compaction_threshold: None,
                request_timeout: Duration::from_secs(2),
                stream_idle_timeout: Duration::from_secs(2),
                max_attempts: 1,
                retry_min_delay: Duration::from_millis(1),
                retry_max_delay: Duration::from_millis(1),
                retry_after_cap: Duration::from_secs(2),
                pricing: crate::usage::PricingCatalog::default(),
                pricing_catalog_url: None,
            },
            AgentConfig {
                context_mode: ContextMode::Stateless,
                context_budget: 8_192,
                max_context_budget: 2_000_000,
                max_tool_iterations: 4,
                workspace_root: workspace.to_path_buf(),
                session_dir: managed.join("sessions"),
                privacy_user_rules_file: managed.join("privacy.ignore"),
                instructions_file: workspace.join("instructions.md"),
                instructions: "test instructions".to_owned(),
                project_instructions: ProjectInstructionsConfig::default(),
                skills: SkillsConfig {
                    enabled: false,
                    ..SkillsConfig::default()
                },
                exec_timeout: Duration::from_secs(2),
                subagents: SubagentConfig {
                    enabled: true,
                    allow_mcp: false,
                    worktree_dir: managed.to_path_buf(),
                    max_parallel: 1,
                    max_per_session: 2,
                    max_tool_iterations: 2,
                    max_tokens_per_agent: 150_000,
                    max_total_tokens_per_session: 500_000,
                    max_depth: 3,
                    max_children_per_agent: 4,
                    task_timeout: Duration::from_secs(10),
                    git_timeout: Duration::from_secs(10),
                },
                shell: ShellConfig::default(),
                whip: WhipConfig::default(),
            },
        )
    }

    fn completed_sse() -> &'static str {
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Recovered safely\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{",
            "\"id\":\"recovery-response\",\"status\":\"completed\",\"created_at\":124,",
            "\"output\":[{\"type\":\"message\",\"id\":\"m1\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"Recovered safely\"}]}],",
            "\"usage\":{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}\n\n",
            "data: [DONE]\n\n"
        )
    }

    fn recursive_spawn_sse() -> String {
        let arguments = serde_json::json!({
            "task": "child recursive inspection",
            "profile_id": "builtin:research",
            "depends_on": [],
            "file_claims": []
        })
        .to_string();
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "type": "response.completed",
                "response": {
                    "id": "recursive-spawn-response",
                    "status": "completed",
                    "created_at": 125,
                    "output": [{
                        "type": "function_call",
                        "call_id": "call_recursive_spawn",
                        "name": "spawn_agent",
                        "arguments": arguments
                    }],
                    "usage": {"input_tokens": 11, "output_tokens": 6, "total_tokens": 17}
                }
            })
        )
    }

    fn writer_outside_claim_sse() -> &'static str {
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"<write_file><path>outside.rs</path><content>outside\\n</content></write_file>\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{",
            "\"id\":\"claim-tool-response\",\"status\":\"completed\",\"created_at\":125,",
            "\"output\":[{\"type\":\"message\",\"id\":\"m2\",\"role\":\"assistant\",",
            "\"content\":[{\"type\":\"output_text\",\"text\":\"<write_file><path>outside.rs</path><content>outside\\n</content></write_file>\"}]}],",
            "\"usage\":{\"input_tokens\":11,\"output_tokens\":6,\"total_tokens\":17}}}\n\n",
            "data: [DONE]\n\n"
        )
    }

    fn worktree_path(path: &Path) -> Option<&str> {
        path.to_str()
    }
}
