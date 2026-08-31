use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{FutureExt, StreamExt, future::join_all};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::{
    api::{
        client::ResponsesClient,
        types::{
            ContextManagement, FunctionCall, FunctionToolDefinition, InputItems, InputMessage,
            ReasoningEffort, ResponsesRequest, ResponsesResponse, StreamEvent,
        },
        validate_completed_status,
    },
    attachments::{AttachmentSource, AttachmentStore, MAX_ATTACHMENTS_PER_TURN},
    code_index::{
        CodeIndexConfig, CodeIndexHit, CodeIndexManager, CodeIndexSnapshot, is_code_index_function,
    },
    config::{AgentConfig, ApiConfig, ApiProvider, ContextMode, WhipConfig},
    error::{ApiError, AppError},
    github::{GitHubConfig, GitHubManager, GitHubSnapshot},
    lsp::{LspConfig, LspDiagnostic, LspManager, LspServerSnapshot, is_lsp_function},
    mcp::{
        McpCallOutput, McpConfig, McpConnectionState, McpManager, McpOAuthPrompt,
        McpPermissionDecision, McpServerSnapshot,
    },
    notice::UiNotice,
    parser::{
        LivePreview, ParserEvent, ScanItem, TagScanner, ToolAction, ToolOutcome, parse_turn,
        visible_assistant_text,
    },
    plugins::{PluginManager, PluginSnapshot},
    privacy::{PrivacyShield, PrivacySnapshot},
    tools::{
        ApprovalBinding, ApprovalNonce, CommandApproval, CommandDigest, ConfirmationDecision,
        ConfirmationReason, DEFAULT_MAX_OUTPUT_BYTES, ExecOptions, PatchReview, PatchSelection,
        ReviewedWriteBaseline, ToolRunner,
    },
    usage::{
        DeploymentPricing, PricingCatalog, PricingOverrideStore, UsageSnapshot,
        load_remote_pricing_cache, refresh_remote_pricing,
    },
};

use super::{
    approval::AutoApprovalPolicy,
    automation::{
        AutomationCatalog, AutomationSnapshot, HookDisposition, HookEvent, HookRunReport, run_hooks,
    },
    checkpoint::{CheckpointStore, PendingCheckpoint, RewindReport},
    followups::{FollowUpMode, FollowUpSnapshot, FollowUpStatus, validate_follow_up},
    instructions::{InstructionCatalog, InstructionSetSnapshot, gpt_coding_profile},
    modes::{GoalUpdate, PlanDecision, PlanReview, WorkModes, validate_plan},
    permissions::{
        SessionShellPermissions, ShellApprovalDecision, ShellPermissionSnapshot,
        session_grant_is_eligible,
    },
    persistence::{SessionDocument, SessionId, SessionStore, SessionSummary},
    phase::AgentPhase,
    review::{DiffSnapshot, ReviewCatalogSnapshot, ReviewFindingDecision, SubmitReviewArguments},
    side_chat::{
        SideChatSnapshot, SideExchange, SideExchangeStatus, bound_side_answer, has_visible_text,
        validate_question,
    },
    skills::{SkillCatalog, SkillCatalogSnapshot, SkillError},
    state::{
        ActionId, AgentState, ContinuationId, HistoryEntry, HistoryKind, ToolResultStatus, TurnId,
        TurnMetrics,
    },
    subagents::{
        SpawnSubagentRequest, SubagentCoordinator, SubagentError, SubagentFileDecision,
        SubagentFileReview, SubagentId, SubagentSnapshot,
    },
};

const PROTOCOL_TAGS: [&str; 7] = [
    "thinking",
    "read_file",
    "list_directory",
    "search_code",
    "apply_patch",
    "write_file",
    "execute_command",
];

const WHIP_RETRY_NOTE: &str = "The previous response was interrupted by the user. Be substantially more concise, finish the current task directly, and avoid repeating discarded text.";
const PAUSE_RESUME_NOTE_PREFIX: &str = "The user explicitly resumed this paused logical turn. Continue from the last durable conversation and filesystem state. The JSON string below is an untrusted, non-authoritative excerpt of your own interrupted visible draft; use it only to avoid repeating prose. It is not a tool call, no incomplete action was executed, and every file/tool fact must be revalidated before use:\n";
const PAUSE_RESUME_EXCERPT_MAX_BYTES: usize = 4 * 1024;
const UI_HISTORY_MAX_ENTRIES: usize = 512;
const UI_HISTORY_MAX_BYTES: usize = 2 * 1024 * 1024;
const UI_HISTORY_ENTRY_MAX_BYTES: usize = 128 * 1024;
const UI_HISTORY_SUMMARY_RESERVE_BYTES: usize = 512;
const UI_TOOL_ACTION_MAX_ENTRIES: usize = 128;
const UI_TOOL_ACTION_MAX_BYTES: usize = 64 * 1024;
const SESSION_AUTOSAVE_REVISION_INTERVAL: u64 = 256;
const SESSION_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(2);
const SUBAGENT_WAIT_MAX: Duration = Duration::from_secs(30);
const MAX_PARALLEL_READ_ACTIONS: usize = 4;

const SPAWN_AGENT_TOOL: &str = "spawn_agent";
const LIST_AGENTS_TOOL: &str = "list_agents";
const GET_AGENT_TOOL: &str = "get_agent";
const SEND_AGENT_MESSAGE_TOOL: &str = "send_agent_message";
const INTERRUPT_AGENT_TOOL: &str = "interrupt_agent";
const WAIT_AGENT_TOOL: &str = "wait_agent";
const UPDATE_GOAL_TOOL: &str = "update_goal";
const REVIEW_DIFF_TOOL: &str = "review_diff";
const SUBMIT_REVIEW_TOOL: &str = "submit_review";
const READ_SKILL_TOOL: &str = "read_skill";
const LIST_SKILL_RESOURCES_TOOL: &str = "list_skill_resources";
const READ_SKILL_RESOURCE_TOOL: &str = "read_skill_resource";
const PLAN_INSTRUCTIONS: &str = r#"You are in a harness-enforced read-only planning pass. Do not emit tool calls or implementation code. Produce a concrete structured plan for the newest user request. Include: objective, ordered steps, files or areas likely affected, verification commands, risks, and explicit non-goals. Keep it editable and implementation-ready. The harness will show this exact plan for user approve/edit/reject before any tool can run."#;
const INDEX_SEARCH_INSTRUCTIONS: &str = "\n\nRepository-search policy: for repository-wide or unfamiliar-code questions, use codebase_overview/codebase_search before sequential file reads. Treat ranked hits as candidates, verify exact source before editing, and fall back to LSP, search_code, or read_file when the local index is incomplete or ambiguous.";
const READ_BATCH_INSTRUCTIONS: &str = "\n\nRead batching policy: when two or more independent read_file, list_directory, or search_code calls are already known, emit up to four of them in the same response. The harness runs only those read-only calls concurrently, isolates individual failures, and returns results in source order. Keep dependent reads, adaptive investigation, parse-error boundaries, approvals, shell commands, and every mutation sequential.";
const SIDE_QUESTION_MAX_OUTPUT_TOKENS: u32 = 2_048;
const MAX_EXACT_COMPACTION_PROBES: usize = 6;
const SIDE_QUESTION_INSTRUCTIONS: &str = r#"SIDE QUESTION CHANNEL (read-only and disposable). You are answering a side question about the current coding session. You MUST NOT call or imitate any tool, emit XML tool tags, edit files, update goals, or steer the main task. Use only the supplied committed conversation context. Be concise, identify uncertainty, and label claims that would require repository verification. This answer is provisional until the user explicitly promotes it into the main conversation."#;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpawnAgentArguments {
    task: String,
    profile_id: String,
    #[serde(default)]
    depends_on: Vec<u64>,
    #[serde(default)]
    file_claims: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyNativeArguments {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRevisionArguments {
    agent_id: u64,
    revision: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMessageArguments {
    agent_id: u64,
    revision: u64,
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitAgentArguments {
    agent_id: u64,
    revision: u64,
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewDiffArguments {
    offset: usize,
    max_bytes: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillIdArguments {
    skill_id: String,
}

#[derive(Debug, Error)]
enum SkillCallError {
    #[error("invalid {function} arguments: {source}")]
    Arguments {
        function: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Catalog(#[from] SkillError),
    #[error("could not encode skill output: {0}")]
    Encode(#[from] serde_json::Error),
    #[error("unknown built-in skill function {function:?}")]
    UnknownFunction { function: String },
    #[error("skill reader task failed: {0}")]
    Worker(#[from] tokio::task::JoinError),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillResourceArguments {
    skill_id: String,
    path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhipKind {
    Soft,
    Hard,
}

#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    PhaseChanged {
        turn_id: Option<TurnId>,
        phase: AgentPhase,
    },
    ThinkingDelta {
        turn_id: TurnId,
        delta: String,
    },
    AssistantCommitted {
        turn_id: TurnId,
        content: String,
    },
    AssistantInterrupted {
        turn_id: TurnId,
        content: String,
    },
    ToolStarted {
        conversation_epoch: u64,
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
    },
    ToolCompleted {
        conversation_epoch: u64,
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        outcome: ToolOutcome,
    },
    McpToolStarted {
        conversation_epoch: u64,
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
    },
    McpToolCompleted {
        conversation_epoch: u64,
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
        outcome: McpCallOutput,
    },
    McpConfirmationRequested {
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
        reason: String,
    },
    McpServersUpdated(Arc<[McpServerSnapshot]>),
    McpOAuthPrompted(McpOAuthPrompt),
    ConfirmationRequested {
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        command: String,
        command_bytes: usize,
        command_digest: CommandDigest,
        model_requested: bool,
        reason: ConfirmationReason,
        session_trust_available: bool,
    },
    PatchApprovalRequested {
        turn_id: TurnId,
        action_id: ActionId,
        review: Arc<PatchReview>,
    },
    ContinuationRequested {
        turn_id: TurnId,
        continuation_id: ContinuationId,
        completed_iterations: u32,
        max_iterations: u32,
    },
    WhipAcknowledged {
        conversation_epoch: u64,
        turn_id: TurnId,
        kind: WhipKind,
    },
    ResetAcknowledged {
        conversation_epoch: u64,
    },
    CheckpointsUpdated(Arc<[super::checkpoint::CheckpointSummary]>),
    RewindCompleted {
        conversation_epoch: u64,
        report: RewindReport,
        history: Arc<[HistoryEntry]>,
    },
    SessionsUpdated {
        sessions: Arc<[SessionSummary]>,
        current_session_id: Option<SessionId>,
    },
    SessionActivated {
        conversation_epoch: u64,
        summary: SessionSummary,
        history: Arc<[HistoryEntry]>,
        usage: UsageSnapshot,
        side_chat: SideChatSnapshot,
        follow_ups: FollowUpSnapshot,
        paused_turn_id: Option<TurnId>,
        context_budget: u32,
    },
    RuntimeSettingsUpdated {
        deployment: String,
        reasoning_effort: ReasoningEffort,
        context_budget: u32,
    },
    HistorySnapshot(Arc<[HistoryEntry]>),
    Usage {
        turn_id: TurnId,
        usage: UsageSnapshot,
    },
    RetryScheduled {
        conversation_epoch: u64,
        turn_id: TurnId,
        next_attempt: u32,
        max_attempts: u32,
        reason: String,
    },
    BusyRejected {
        turn_id: TurnId,
        message: String,
    },
    RecoverableError {
        turn_id: Option<TurnId>,
        message: String,
    },
    FatalError {
        message: String,
    },
    Done {
        turn_id: TurnId,
    },
    TurnPaused {
        turn_id: TurnId,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WhipTelemetry {
    pub total_strikes: u64,
    pub penalty_responses_remaining: u32,
    /// Estimated reduction in maximum output-token budget, not metered usage.
    pub estimated_saved_token_budget: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetrySnapshot {
    pub next_attempt: u32,
    pub max_attempts: u32,
    pub reason: String,
}

/// Immutable native-tool call identity shown to the user before approval.
/// Keeping the Responses `call_id`, function name and resolved MCP identity
/// together prevents a stale dialog from authorizing a different call after a
/// server reconnect or tool-registry refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolCall {
    pub call_id: String,
    pub function_name: String,
    pub server: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub enum UiModal {
    PlanApproval {
        review: Arc<PlanReview>,
    },
    Confirmation {
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        command: String,
        command_bytes: usize,
        command_digest: CommandDigest,
        model_requested: bool,
        reason: ConfirmationReason,
        session_trust_available: bool,
    },
    McpConfirmation {
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
        reason: String,
    },
    PatchApproval {
        turn_id: TurnId,
        action_id: ActionId,
        review: Arc<PatchReview>,
    },
    Continuation {
        turn_id: TurnId,
        continuation_id: ContinuationId,
        completed_iterations: u32,
        max_iterations: u32,
    },
    SubagentPatchApproval {
        review: Arc<SubagentFileReview>,
    },
}

/// Latest-only view of actor state. Slow terminals consume this watch value
/// without ever back-pressuring HTTP, parsing, cancellation, or tool cleanup.
/// `OrchestratorEvent` is retained as a small best-effort diagnostics/ack
/// stream for tests and integrations that need edge notifications.
#[derive(Debug, Clone)]
pub struct UiSnapshot {
    pub conversation_epoch: u64,
    pub phase_revision: u64,
    pub history_revision: u64,
    pub phase: AgentPhase,
    pub active_turn_id: Option<TurnId>,
    pub paused_turn_id: Option<TurnId>,
    pub history: Arc<[HistoryEntry]>,
    pub checkpoints: Arc<[super::checkpoint::CheckpointSummary]>,
    pub sessions: Arc<[SessionSummary]>,
    pub current_session_id: Option<SessionId>,
    pub deployment: String,
    pub reasoning_effort: ReasoningEffort,
    pub context_budget: u32,
    pub max_context_budget: u32,
    pub work_modes: WorkModes,
    pub auto_approval: AutoApprovalPolicy,
    pub instructions: InstructionSetSnapshot,
    pub skills: SkillCatalogSnapshot,
    pub automation: AutomationSnapshot,
    pub plugins: PluginSnapshot,
    /// Latest bounded display artifacts keyed by action ID. This lives in the
    /// watch snapshot so a full best-effort diagnostic channel cannot make an
    /// expandable tool row lose its arguments or patch diff.
    pub tool_actions: Arc<BTreeMap<ActionId, ToolAction>>,
    pub mcp_calls: Arc<BTreeMap<ActionId, Arc<McpToolCall>>>,
    pub thinking: String,
    pub assistant: String,
    pub interrupted_draft: String,
    pub modal: Option<UiModal>,
    pub usage: Option<UsageSnapshot>,
    pub whip: WhipTelemetry,
    pub connection_status: String,
    pub retry: Option<RetrySnapshot>,
    pub mcp_servers: Arc<[McpServerSnapshot]>,
    pub mcp_oauth_prompt: Option<McpOAuthPrompt>,
    pub lsp_servers: Arc<[LspServerSnapshot]>,
    pub lsp_diagnostics: Arc<[LspDiagnostic]>,
    pub code_index: CodeIndexSnapshot,
    pub github: GitHubSnapshot,
    pub code_index_hits: Arc<[CodeIndexHit]>,
    pub privacy: PrivacySnapshot,
    pub shell_permissions: ShellPermissionSnapshot,
    pub side_chat: SideChatSnapshot,
    pub follow_ups: FollowUpSnapshot,
    pub reviews: ReviewCatalogSnapshot,
    #[doc(hidden)]
    pub side_task_generation: u64,
    pub subagents: super::subagents::SubagentFleetSnapshot,
    /// Compatibility/diagnostic text retained for integration consumers. The
    /// TUI renders only `notice`, whose application-owned variants localize at
    /// the presentation boundary.
    pub status: String,
    pub notice: UiNotice,
}

impl Default for UiSnapshot {
    fn default() -> Self {
        Self {
            conversation_epoch: 1,
            phase_revision: 0,
            history_revision: 0,
            phase: AgentPhase::Idle,
            active_turn_id: None,
            paused_turn_id: None,
            history: Arc::from([]),
            checkpoints: Arc::from([]),
            sessions: Arc::from([]),
            current_session_id: None,
            deployment: String::new(),
            reasoning_effort: ReasoningEffort::Medium,
            context_budget: 0,
            max_context_budget: crate::config::MAX_CONTEXT_BUDGET,
            work_modes: WorkModes::default(),
            auto_approval: AutoApprovalPolicy::default(),
            instructions: InstructionSetSnapshot::default(),
            skills: SkillCatalogSnapshot::default(),
            automation: AutomationSnapshot::default(),
            plugins: PluginSnapshot::default(),
            tool_actions: Arc::new(BTreeMap::new()),
            mcp_calls: Arc::new(BTreeMap::new()),
            thinking: String::new(),
            assistant: String::new(),
            interrupted_draft: String::new(),
            modal: None,
            usage: None,
            whip: WhipTelemetry::default(),
            connection_status: "idle".to_owned(),
            retry: None,
            mcp_servers: Arc::from([]),
            mcp_oauth_prompt: None,
            lsp_servers: Arc::from([]),
            lsp_diagnostics: Arc::from([]),
            code_index: CodeIndexSnapshot::new(false),
            github: GitHubSnapshot::default(),
            code_index_hits: Arc::from([]),
            privacy: PrivacySnapshot::default(),
            shell_permissions: ShellPermissionSnapshot::default(),
            side_chat: SideChatSnapshot::default(),
            follow_ups: FollowUpSnapshot::default(),
            reviews: ReviewCatalogSnapshot::default(),
            side_task_generation: 0,
            subagents: super::subagents::SubagentFleetSnapshot::default(),
            status: String::new(),
            notice: UiNotice::None,
        }
    }
}

#[derive(Debug)]
pub enum OrchestratorCommand {
    Submit {
        prompt: String,
        attachments: Vec<AttachmentSource>,
        scope: CommandScope,
    },
    Confirm {
        turn_id: TurnId,
        action_id: ActionId,
        decision: ShellApprovalDecision,
    },
    DecidePatch {
        turn_id: TurnId,
        action_id: ActionId,
        decisions: Vec<bool>,
    },
    Whip {
        turn_id: TurnId,
    },
    Interrupt {
        turn_id: TurnId,
    },
    ContinueToolLoop {
        turn_id: TurnId,
        continuation_id: ContinuationId,
        continue_loop: bool,
    },
    RetryTurn {
        turn_id: TurnId,
    },
    AbortTurn {
        turn_id: TurnId,
    },
    Reset,
    Rewind {
        checkpoint_id: u64,
        scope: CommandScope,
    },
    RefreshSessions {
        query: String,
        include_archived: bool,
    },
    NewSession {
        scope: CommandScope,
    },
    ResumeSession {
        session_id: SessionId,
        allow_workspace_mismatch: bool,
        scope: CommandScope,
    },
    ForkSession {
        session_id: SessionId,
        scope: CommandScope,
    },
    RenameSession {
        session_id: SessionId,
        title: String,
        scope: CommandScope,
    },
    SetSessionPinned {
        session_id: SessionId,
        pinned: bool,
        scope: CommandScope,
    },
    SetSessionArchived {
        session_id: SessionId,
        archived: bool,
        scope: CommandScope,
    },
    UpdateRuntimeSettings {
        deployment: String,
        reasoning_effort: ReasoningEffort,
        deep_thinking: bool,
        context_budget: u32,
        scope: CommandScope,
    },
    SetDeploymentPricing {
        pricing: DeploymentPricing,
        scope: CommandScope,
    },
    RemoveDeploymentPricing {
        deployment: String,
        scope: CommandScope,
    },
    GitHubRefresh {
        scope: CommandScope,
    },
    GitHubOpen {
        number: u64,
        scope: CommandScope,
    },
    GitHubCheckout {
        number: u64,
        scope: CommandScope,
    },
    GitHubCreateDraft {
        scope: CommandScope,
    },
    SetPlanMode {
        enabled: bool,
        scope: CommandScope,
    },
    SetExploreMode {
        enabled: bool,
        scope: CommandScope,
    },
    SetReviewMode {
        enabled: bool,
        scope: CommandScope,
    },
    SetDeepThinkingMode {
        enabled: bool,
        scope: CommandScope,
    },
    SetAutoApprovalPolicy {
        policy: AutoApprovalPolicy,
        scope: CommandScope,
    },
    SetGoal {
        objective: Option<String>,
        scope: CommandScope,
    },
    ReloadProjectInstructions {
        scope: CommandScope,
    },
    SetProjectInstructionsEnabled {
        enabled: bool,
        scope: CommandScope,
    },
    SetInstructionSourceEnabled {
        id: String,
        enabled: bool,
        scope: CommandScope,
    },
    ReloadSkills {
        scope: CommandScope,
    },
    SetSkillEnabled {
        id: String,
        enabled: bool,
        scope: CommandScope,
    },
    ReloadAutomation {
        scope: CommandScope,
    },
    SetHookEnabled {
        id: String,
        enabled: bool,
        scope: CommandScope,
    },
    RefreshPlugins {
        scope: CommandScope,
    },
    AddPluginMarketplace {
        source: String,
        scope: CommandScope,
    },
    RemovePluginMarketplace {
        source: String,
        scope: CommandScope,
    },
    InstallLocalPlugin {
        package: String,
        scope: CommandScope,
    },
    InstallMarketplacePlugin {
        id: String,
        scope: CommandScope,
    },
    UpdatePlugin {
        id: String,
        scope: CommandScope,
    },
    SetPluginEnabled {
        id: String,
        enabled: bool,
        scope: CommandScope,
    },
    RemovePlugin {
        id: String,
        scope: CommandScope,
    },
    DecidePlan {
        turn_id: TurnId,
        review_id: u64,
        decision: PlanDecision,
    },
    McpConnect {
        server: String,
        scope: CommandScope,
    },
    McpDisconnect {
        server: String,
        scope: CommandScope,
    },
    McpSetEnabled {
        server: String,
        enabled: bool,
        scope: CommandScope,
    },
    McpAddServer {
        server: crate::mcp::McpServerConfig,
        scope: CommandScope,
    },
    SetSubagentMcpAccess {
        enabled: bool,
        scope: CommandScope,
    },
    McpBeginOAuth {
        server: String,
        scope: CommandScope,
    },
    McpPollOAuth {
        server: String,
        scope: CommandScope,
    },
    McpForgetOAuth {
        server: String,
        scope: CommandScope,
    },
    LspConnect {
        server: String,
        scope: CommandScope,
    },
    LspDisconnect {
        server: String,
        scope: CommandScope,
    },
    LspSetEnabled {
        server: String,
        enabled: bool,
        scope: CommandScope,
    },
    LspAddServer {
        server: crate::lsp::LspServerConfig,
        scope: CommandScope,
    },
    LspRefresh {
        scope: CommandScope,
    },
    CodeIndexRefresh {
        force: bool,
        scope: CommandScope,
    },
    CodeIndexCancel {
        scope: CommandScope,
    },
    CodeIndexPoll {
        scope: CommandScope,
    },
    CodeIndexSearch {
        query: String,
        path: Option<String>,
        top: usize,
        scope: CommandScope,
    },
    ReloadPrivacy {
        scope: CommandScope,
    },
    RevokeSessionShellGrant {
        grant_id: u64,
        scope: CommandScope,
    },
    ClearSessionShellGrants {
        scope: CommandScope,
    },
    AskSideQuestion {
        question: String,
        deployment: String,
        reasoning_effort: ReasoningEffort,
        scope: CommandScope,
    },
    CancelSideQuestion {
        request_id: u64,
        scope: CommandScope,
    },
    EnqueueFollowUp {
        mode: FollowUpMode,
        text: String,
        scope: CommandScope,
    },
    EditFollowUp {
        id: u64,
        revision: u64,
        text: String,
        scope: CommandScope,
    },
    CancelFollowUp {
        id: u64,
        revision: u64,
        scope: CommandScope,
    },
    RetryFollowUp {
        id: u64,
        revision: u64,
        scope: CommandScope,
    },
    DispatchFollowUpQueue {
        scope: CommandScope,
    },
    DecideReviewFinding {
        report_id: u64,
        revision: u64,
        finding_id: u64,
        decision: ReviewFindingDecision,
        scope: CommandScope,
    },
    SpawnSubagent {
        task: String,
        profile_id: String,
        dependencies: Vec<SubagentId>,
        file_claims: Vec<String>,
    },
    ReloadSubagentProfiles,
    MessageSubagent {
        agent_id: SubagentId,
        expected_revision: u64,
        message: String,
    },
    CancelSubagent {
        agent_id: SubagentId,
        expected_revision: u64,
    },
    ResumeSubagent {
        agent_id: SubagentId,
        expected_revision: u64,
    },
    AbandonSubagentRecovery {
        agent_id: SubagentId,
        expected_revision: u64,
    },
    DecideSubagentCommand {
        agent_id: SubagentId,
        expected_revision: u64,
        action_id: u64,
        approved: bool,
    },
    DecideSubagentBudget {
        agent_id: SubagentId,
        expected_revision: u64,
        approved: bool,
    },
    OpenSubagentReview {
        agent_id: SubagentId,
        expected_revision: u64,
        change_digest: String,
        path: String,
        scope: CommandScope,
    },
    DecideSubagentFile {
        review: Arc<SubagentFileReview>,
        decision: SubagentFileDecision,
        scope: CommandScope,
    },
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandScope {
    pub conversation_epoch: u64,
    pub phase_revision: u64,
}

const URGENT_CONTROL_CAPACITY: usize = 8;

#[derive(Debug, Clone, Copy)]
enum UrgentControlKind {
    Whip { turn_id: TurnId },
    Interrupt { turn_id: TurnId },
    Reset,
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
struct UrgentControlSignal {
    sequence: u64,
    timestamp: Instant,
    kind: UrgentControlKind,
}

#[derive(Debug, Default)]
struct UrgentControlState {
    next_sequence: u64,
    signals: VecDeque<UrgentControlSignal>,
    sticky_reset: Option<UrgentControlSignal>,
    sticky_shutdown: Option<UrgentControlSignal>,
    pause_requests: BTreeSet<TurnId>,
}

#[derive(Debug, Default)]
struct UrgentControlPlane {
    state: Mutex<UrgentControlState>,
    active_turn: Mutex<Option<(TurnId, CancellationToken)>>,
    notify: Notify,
}

/// Out-of-band, bounded control path for actions that must preempt network or
/// tool awaits. Signals carry actor-independent timestamps and monotonically
/// increasing sequence numbers; repeated signals are coalesced in a tiny
/// in-memory state instead of competing with Submit/Confirm queue capacity.
#[derive(Debug, Clone, Default)]
pub struct UrgentControlHandle {
    inner: Arc<UrgentControlPlane>,
}

impl UrgentControlHandle {
    pub fn whip(&self, turn_id: TurnId) -> u64 {
        self.push(UrgentControlKind::Whip { turn_id })
    }

    pub fn interrupt(&self, turn_id: TurnId) -> u64 {
        self.push(UrgentControlKind::Interrupt { turn_id })
    }

    /// Cancel the active operation like Interrupt, but retain an explicit,
    /// durable intent to resume this logical turn from its last committed
    /// harness boundary.
    pub fn pause(&self, turn_id: TurnId) -> u64 {
        if let Ok(mut state) = self.inner.state.lock() {
            state.pause_requests.insert(turn_id);
        } else {
            tracing::error!("urgent control state lock was poisoned");
            return 0;
        }
        self.push(UrgentControlKind::Interrupt { turn_id })
    }

    pub fn reset(&self) -> u64 {
        self.push(UrgentControlKind::Reset)
    }

    pub fn shutdown(&self) -> u64 {
        self.push(UrgentControlKind::Shutdown)
    }

    fn push(&self, kind: UrgentControlKind) -> u64 {
        let Ok(mut state) = self.inner.state.lock() else {
            tracing::error!("urgent control state lock was poisoned");
            return 0;
        };
        state.next_sequence = state.next_sequence.saturating_add(1);
        let sequence = state.next_sequence;
        let signal = UrgentControlSignal {
            sequence,
            timestamp: Instant::now(),
            kind,
        };

        match kind {
            UrgentControlKind::Shutdown => {
                state.sticky_shutdown = Some(signal);
                state.sticky_reset = None;
                state.signals.clear();
                state.pause_requests.clear();
            }
            UrgentControlKind::Reset if state.sticky_shutdown.is_none() => {
                state.sticky_reset = Some(signal);
                state.signals.clear();
                state.pause_requests.clear();
            }
            UrgentControlKind::Reset => {}
            UrgentControlKind::Interrupt { turn_id }
                if state.sticky_shutdown.is_none() && state.sticky_reset.is_none() =>
            {
                state.signals.retain(|signal| {
                    !matches!(
                        signal.kind,
                        UrgentControlKind::Interrupt { turn_id: existing } if existing == turn_id
                    )
                });
                state.signals.push_back(signal);
            }
            UrgentControlKind::Interrupt { .. } => {}
            UrgentControlKind::Whip { turn_id } => {
                if state.sticky_shutdown.is_none() && state.sticky_reset.is_none() {
                    let matching = state
                        .signals
                        .iter()
                        .filter(|signal| {
                            matches!(
                                signal.kind,
                                UrgentControlKind::Whip { turn_id: existing } if existing == turn_id
                            )
                        })
                        .count();
                    if matching >= 2
                        && let Some(position) = state.signals.iter().position(|signal| {
                            matches!(
                                signal.kind,
                                UrgentControlKind::Whip { turn_id: existing } if existing == turn_id
                            )
                        })
                    {
                        state.signals.remove(position);
                    }
                    state.signals.push_back(signal);
                }
            }
        }

        while state.signals.len() > URGENT_CONTROL_CAPACITY {
            state.signals.pop_front();
        }
        drop(state);
        match kind {
            UrgentControlKind::Interrupt { turn_id } => {
                if let Ok(active) = self.inner.active_turn.lock()
                    && let Some((active_turn, cancel)) = active.as_ref()
                    && *active_turn == turn_id
                {
                    cancel.cancel();
                }
            }
            UrgentControlKind::Reset | UrgentControlKind::Shutdown => {
                if let Ok(active) = self.inner.active_turn.lock()
                    && let Some((_, cancel)) = active.as_ref()
                {
                    cancel.cancel();
                }
            }
            UrgentControlKind::Whip { .. } => {}
        }
        self.inner.notify.notify_one();
        sequence
    }

    async fn notified(&self) {
        self.inner.notify.notified().await;
    }

    fn drain(&self) -> Vec<UrgentControlSignal> {
        let Ok(mut state) = self.inner.state.lock() else {
            tracing::error!("urgent control state lock was poisoned");
            return Vec::new();
        };
        if let Some(shutdown) = state.sticky_shutdown.take() {
            state.sticky_reset = None;
            state.signals.clear();
            return vec![shutdown];
        }
        if let Some(reset) = state.sticky_reset.take() {
            state.signals.clear();
            return vec![reset];
        }
        let mut signals: Vec<_> = state.signals.drain(..).collect();
        signals.sort_by_key(|signal| signal.sequence);
        signals
    }

    fn activate_turn(&self, turn_id: TurnId, cancel: CancellationToken) {
        let Ok(mut active) = self.inner.active_turn.lock() else {
            tracing::error!("urgent active-turn lock was poisoned");
            cancel.cancel();
            return;
        };
        *active = Some((turn_id, cancel));
    }

    fn clear_turn(&self, turn_id: TurnId) {
        let Ok(mut active) = self.inner.active_turn.lock() else {
            tracing::error!("urgent active-turn lock was poisoned");
            return;
        };
        if active
            .as_ref()
            .is_some_and(|(active_turn, _)| *active_turn == turn_id)
        {
            *active = None;
        }
    }

    fn take_pause_request(&self, turn_id: TurnId) -> bool {
        let Ok(mut state) = self.inner.state.lock() else {
            tracing::error!("urgent control state lock was poisoned");
            return false;
        };
        state.pause_requests.remove(&turn_id)
    }
}

pub struct Orchestrator {
    client: Arc<ResponsesClient>,
    attachment_store: AttachmentStore,
    tool_runner: Arc<ToolRunner>,
    subagents: SubagentCoordinator,
    mcp: Option<McpManager>,
    lsp: Option<LspManager>,
    managed_connections: crate::managed_connections::ManagedConnectionStore,
    code_index: Option<CodeIndexManager>,
    github: Option<GitHubManager>,
    privacy: PrivacyShield,
    session_shell_permissions: SessionShellPermissions,
    agent_config: AgentConfig,
    default_context_budget: u32,
    instructions: InstructionCatalog,
    skills: SkillCatalog,
    automation: Arc<Mutex<AutomationCatalog>>,
    plugins: PluginManager,
    provider: ApiProvider,
    deployment: String,
    base_max_output_tokens: u32,
    base_reasoning_effort: ReasoningEffort,
    temperature: Option<f32>,
    context_management: Option<Vec<ContextManagement>>,
    pricing: PricingCatalog,
    base_pricing: PricingCatalog,
    pricing_overrides: PricingOverrideStore,
    pricing_catalog_url: Option<String>,
    pricing_cache_path: PathBuf,
    deployment_choices: Arc<[String]>,
    state: AgentState,
    event_tx: mpsc::Sender<OrchestratorEvent>,
    snapshot_tx: watch::Sender<UiSnapshot>,
    command_rx: mpsc::Receiver<OrchestratorCommand>,
    urgent_control: UrgentControlHandle,
    side_result_tx: mpsc::Sender<SideTaskResult>,
    side_result_rx: mpsc::Receiver<SideTaskResult>,
    side_cancel: Option<(u64, u64, CancellationToken)>,
    next_side_generation: u64,
    queue_dispatch_request: Option<bool>,
    next_turn_id: TurnId,
    next_action_id: ActionId,
    next_continuation_id: ContinuationId,
    next_plan_review_id: u64,
    active_review: Option<DiffSnapshot>,
    conversation_epoch: u64,
    last_whip: Option<(TurnId, Instant)>,
    penalty_responses_remaining: u32,
    whip_retry_note_pending: Option<TurnId>,
    pause_resume_note_pending: Option<TurnId>,
    retryable_turn: Option<TurnId>,
    checkpoint_store: Option<CheckpointStore>,
    pending_checkpoint: Option<PendingCheckpoint>,
    session_store: SessionStore,
    current_session_id: Option<SessionId>,
    last_session_persist_revision: u64,
    last_session_persist_at: Instant,
}

#[derive(Debug)]
enum TurnExit {
    Completed,
    Interrupted,
    Reset,
    Shutdown,
    Failed(String),
}

#[derive(Debug)]
enum AttemptExit {
    Completed {
        response: ResponsesResponse,
        text: String,
        penalty_applied: bool,
    },
    WhipRetry {
        partial: String,
    },
    Interrupted {
        partial: String,
    },
    Reset {
        partial: String,
    },
    Shutdown,
    Failed {
        partial: String,
        error: ApiError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AwaitedControl {
    Continue,
    Interrupt,
    Reset,
    Shutdown,
}

#[derive(Debug)]
struct SideTaskResult {
    conversation_epoch: u64,
    generation: u64,
    exchange: SideExchange,
}

async fn run_side_question(
    client: Arc<ResponsesClient>,
    request: ResponsesRequest,
    cancel: CancellationToken,
    mut exchange: SideExchange,
) -> SideExchange {
    let completed = match client.completed_response(request, cancel).await {
        Ok(completed) => completed,
        Err(ApiError::Cancelled) => {
            exchange.status = SideExchangeStatus::Cancelled;
            exchange.notice = UiNotice::SideQuestionCancelled;
            exchange.completed_at = Some(chrono::Utc::now());
            return exchange;
        }
        Err(error) => {
            exchange.status = SideExchangeStatus::Failed;
            exchange.notice = UiNotice::external(error.to_string());
            exchange.completed_at = Some(chrono::Utc::now());
            return exchange;
        }
    };
    if let Some(usage) = &completed.response.usage {
        exchange.input_tokens = usage.input_tokens;
        exchange.cached_input_tokens = usage.cached_input_tokens();
        exchange.output_tokens = usage.output_tokens;
        exchange.total_tokens = usage.total_tokens;
    }
    let function_calls = match completed.response.function_calls() {
        Ok(calls) => calls,
        Err(error) => {
            exchange.status = SideExchangeStatus::Failed;
            exchange.notice = UiNotice::external(error.to_string());
            exchange.completed_at = Some(chrono::Utc::now());
            return exchange;
        }
    };
    let attempted_legacy_tool = TagScanner::new(&completed.text).any(|item| match item {
        ScanItem::Block(block) => block.tag.is_tool(),
        ScanItem::Error(error) => error.tag().is_some_and(|tag| tag.is_tool()),
        ScanItem::UnexpectedText { .. } => false,
    });
    if !function_calls.is_empty() || attempted_legacy_tool {
        exchange.status = SideExchangeStatus::Failed;
        exchange.notice = UiNotice::SideToolCallBlocked;
        exchange.completed_at = Some(chrono::Utc::now());
        return exchange;
    }
    let answer = bound_side_answer(visible_assistant_text(&completed.text));
    if !has_visible_text(&answer) {
        exchange.status = SideExchangeStatus::Failed;
        exchange.notice = UiNotice::SideAnswerEmpty;
        exchange.completed_at = Some(chrono::Utc::now());
        return exchange;
    }
    exchange.answer = answer;
    exchange.status = SideExchangeStatus::Completed;
    exchange.notice = UiNotice::SideAnswerProvisional;
    exchange.completed_at = Some(chrono::Utc::now());
    exchange
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryWaitExit {
    Ready,
    Interrupted,
    Reset,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PatchApprovalSummary {
    approved_hunks: usize,
    total_hunks: usize,
}

struct ActionFinalization<'a> {
    checkpoint_before: Option<String>,
    patch_approval: Option<PatchApprovalSummary>,
    hook_cancel: &'a CancellationToken,
}

struct ReadBatchItem {
    action_id: ActionId,
    action: ToolAction,
    outcome: Option<ToolOutcome>,
}

fn is_parallel_read_action(action: &ToolAction) -> bool {
    matches!(
        action,
        ToolAction::ReadFile { .. }
            | ToolAction::ListDirectory { .. }
            | ToolAction::SearchCode { .. }
    )
}

fn collect_parallel_read_batch(
    first: ToolAction,
    pending: &mut VecDeque<ParserEvent>,
) -> Vec<ToolAction> {
    let first_is_parallel = is_parallel_read_action(&first);
    let mut batch = vec![first];
    if !first_is_parallel {
        return batch;
    }
    while batch.len() < MAX_PARALLEL_READ_ACTIONS {
        let Some(ParserEvent::ToolCallParsed(next)) = pending.front() else {
            break;
        };
        if !is_parallel_read_action(next) {
            break;
        }
        let Some(ParserEvent::ToolCallParsed(next)) = pending.pop_front() else {
            break;
        };
        batch.push(next);
    }
    batch
}

fn annotate_patch_outcome(
    outcome: ToolOutcome,
    approval: Option<PatchApprovalSummary>,
) -> ToolOutcome {
    let Some(approval) = approval else {
        return outcome;
    };
    match outcome {
        ToolOutcome::Success(message) => ToolOutcome::Success(format!(
            "{message}\nUser approved {} of {} patch hunks; rejected hunks were left unchanged.",
            approval.approved_hunks, approval.total_hunks
        )),
        other => other,
    }
}

/// Incremental outer-protocol tracker used only to find a safe Soft Whip
/// boundary. It intentionally does not parse tool fields or authorize tools;
/// the authoritative parser still runs once, after a validated completed
/// response. Keeping the partial tag buffer lets a strike in the middle of
/// `</thinking>` observe the closing `>` from a later SSE chunk.
#[derive(Debug, Default)]
struct ProtocolBoundaryTracker {
    outer: Option<&'static str>,
    tag_buffer: String,
    collecting_tag: bool,
}

impl ProtocolBoundaryTracker {
    fn feed(&mut self, chunk: &str) -> bool {
        let mut closed_outer = false;
        for character in chunk.chars() {
            if !self.collecting_tag {
                if character == '<' {
                    self.collecting_tag = true;
                    self.tag_buffer.clear();
                    self.tag_buffer.push(character);
                }
                continue;
            }

            self.tag_buffer.push(character);
            let possible_protocol_tag = if let Some(active) = self.outer {
                format!("</{active}>").starts_with(&self.tag_buffer)
            } else {
                PROTOCOL_TAGS
                    .iter()
                    .any(|name| format!("<{name}>").starts_with(&self.tag_buffer))
            };
            if !possible_protocol_tag {
                self.collecting_tag = false;
                self.tag_buffer.clear();
                continue;
            }
            if self.tag_buffer.len() > 96 {
                self.collecting_tag = false;
                self.tag_buffer.clear();
                continue;
            }
            if character != '>' {
                continue;
            }

            let token = self.tag_buffer.as_str();
            if let Some(active) = self.outer {
                if token
                    .strip_prefix("</")
                    .and_then(|value| value.strip_suffix('>'))
                    == Some(active)
                {
                    self.outer = None;
                    closed_outer = true;
                }
            } else if let Some(name) = token
                .strip_prefix('<')
                .and_then(|value| value.strip_suffix('>'))
                .and_then(protocol_tag)
            {
                self.outer = Some(name);
            }

            self.collecting_tag = false;
            self.tag_buffer.clear();
        }
        closed_outer
    }

    const fn is_at_boundary(&self) -> bool {
        // A partial *opening* marker has not entered a protocol block yet, so
        // discarding the response is already safe. For a split closing marker
        // `outer` remains Some until the final `>` is observed.
        self.outer.is_none()
    }
}

fn protocol_tag(candidate: &str) -> Option<&'static str> {
    PROTOCOL_TAGS
        .iter()
        .copied()
        .find(|known| *known == candidate)
}

fn subagent_function_definitions(profile_ids: &[String]) -> Vec<FunctionToolDefinition> {
    let empty = serde_json::json!({
        "type": "object",
        "properties": {},
        "required": [],
        "additionalProperties": false
    });
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
    if !profile_ids.is_empty() {
        tools.push(FunctionToolDefinition::new(
            SPAWN_AGENT_TOOL,
            Some(
                format!(
                    "Delegate one bounded task to a recursively capable sub-agent using an available profile. Depth, child fan-out, total session count, and concurrency are harness-bounded. Use depends_on for hard DAG prerequisites. Writer profiles edit only an isolated Git worktree; declare precise project-relative file_claims when known so non-overlapping writers can run concurrently. An empty writer claim is safely exclusive. Available profile IDs: {}.",
                    profile_ids.join(", ")
                ),
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string", "minLength": 1 },
                    "profile_id": { "type": "string", "enum": profile_ids },
                    // Structured Outputs does not support JSON Schema's
                    // `uniqueItems`. The scheduler normalizes and validates
                    // both collections before accepting the request.
                    "depends_on": {
                        "type": "array",
                        "maxItems": 32,
                        "items": { "type": "integer", "minimum": 1 }
                    },
                    "file_claims": {
                        "type": "array",
                        "maxItems": 64,
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
            LIST_AGENTS_TOOL,
            Some("List authoritative sub-agent IDs, revisions, actual model/effort, status, and progress.".to_owned()),
            empty.clone(),
        ),
        FunctionToolDefinition::new(
            GET_AGENT_TOOL,
            Some("Read one sub-agent's bounded result, error, progress, and changed-file summary.".to_owned()),
            identity.clone(),
        ),
        FunctionToolDefinition::new(
            SEND_AGENT_MESSAGE_TOOL,
            Some("Send a follow-up to a running sub-agent. The revision prevents acting on stale UI or model state.".to_owned()),
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
            INTERRUPT_AGENT_TOOL,
            Some("Request cancellation of one running sub-agent using its current revision.".to_owned()),
            identity.clone(),
        ),
        FunctionToolDefinition::new(
            WAIT_AGENT_TOOL,
            Some("Wait briefly for one sub-agent revision or terminal status change; returns the latest authoritative snapshot on timeout.".to_owned()),
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

fn goal_update_function_definition() -> FunctionToolDefinition {
    FunctionToolDefinition::new(
        UPDATE_GOAL_TOOL,
        Some(
            "Reconcile persistent Goal Mode progress with evidence from this turn. Call before declaring the turn complete; never claim verification that was not run."
                .to_owned(),
        ),
        serde_json::json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["active", "completed", "blocked"] },
                "summary": { "type": "string", "maxLength": 16384 },
                "completed_steps": {
                    "type": "array",
                    "maxItems": 64,
                    "items": { "type": "string", "minLength": 1, "maxLength": 2048 }
                },
                "next_steps": {
                    "type": "array",
                    "maxItems": 64,
                    "items": { "type": "string", "minLength": 1, "maxLength": 2048 }
                },
                "verification": { "type": "string", "maxLength": 16384 }
            },
            "required": ["status", "summary", "completed_steps", "next_steps", "verification"],
            "additionalProperties": false
        }),
    )
}

fn review_function_definitions() -> [FunctionToolDefinition; 2] {
    [
        FunctionToolDefinition::new(
            REVIEW_DIFF_TOOL,
            Some(
                "Read one UTF-8-safe page from the immutable Git diff captured at the start of Review Mode. Start at offset 0 and continue with next_offset until complete is true."
                    .to_owned(),
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "offset": { "type": "integer", "minimum": 0 },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1024,
                        "maximum": crate::agent::review::MAX_REVIEW_CHUNK_BYTES
                    }
                },
                "required": ["offset", "max_bytes"],
                "additionalProperties": false
            }),
        ),
        FunctionToolDefinition::new(
            SUBMIT_REVIEW_TOOL,
            Some(
                "Submit the one structured, snapshot-bound Review Mode result. Report only concrete defects introduced by the captured diff; use an empty findings array when the verdict is pass."
                    .to_owned(),
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "snapshot_sha256": { "type": "string", "minLength": 64, "maxLength": 64 },
                    "verdict": { "type": "string", "enum": ["pass", "changes_requested"] },
                    "summary": { "type": "string", "minLength": 1, "maxLength": 32768 },
                    "findings": {
                        "type": "array",
                        "maxItems": crate::agent::review::MAX_REVIEW_FINDINGS,
                        "items": {
                            "type": "object",
                            "properties": {
                                "severity": {
                                    "type": "string",
                                    "enum": ["low", "medium", "high", "critical"]
                                },
                                "title": { "type": "string", "minLength": 1, "maxLength": 512 },
                                "body": { "type": "string", "minLength": 1, "maxLength": 16384 },
                                "path": { "type": "string", "minLength": 1, "maxLength": 4096 },
                                "line_start": { "type": ["integer", "null"], "minimum": 1 },
                                "line_end": { "type": ["integer", "null"], "minimum": 1 },
                                "suggested_fix": { "type": "string", "maxLength": 16384 }
                            },
                            "required": [
                                "severity", "title", "body", "path", "line_start", "line_end",
                                "suggested_fix"
                            ],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["snapshot_sha256", "verdict", "summary", "findings"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn skill_function_definitions() -> [FunctionToolDefinition; 3] {
    let skill_id = serde_json::json!({
        "type": "object",
        "properties": {
            "skill_id": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "Exact id from <available_skills>; do not guess names or paths."
            }
        },
        "required": ["skill_id"],
        "additionalProperties": false
    });
    [
        FunctionToolDefinition::new(
            READ_SKILL_TOOL,
            Some(
                "Read one enabled SKILL.md after its metadata is relevant. The file is bounded and revalidated against symlink, root, UTF-8, and privacy policy at call time. Reading instructions never authorizes bundled commands or scripts."
                    .to_owned(),
            ),
            skill_id.clone(),
        ),
        FunctionToolDefinition::new(
            LIST_SKILL_RESOURCES_TOOL,
            Some(
                "List bounded regular files bundled with one enabled skill. Paths are relative to that skill and symlinks are excluded."
                    .to_owned(),
            ),
            skill_id,
        ),
        FunctionToolDefinition::new(
            READ_SKILL_RESOURCE_TOOL,
            Some(
                "Read one UTF-8 resource previously returned by list_skill_resources. Traversal, absolute paths, symlinks, oversized files, and privacy-blocked project files are rejected."
                    .to_owned(),
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "skill_id": { "type": "string", "minLength": 1, "maxLength": 4096 },
                    "path": { "type": "string", "minLength": 1, "maxLength": 4096 }
                },
                "required": ["skill_id", "path"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn is_skill_function(name: &str) -> bool {
    matches!(
        name,
        READ_SKILL_TOOL | LIST_SKILL_RESOURCES_TOOL | READ_SKILL_RESOURCE_TOOL
    )
}

fn is_subagent_function(name: &str) -> bool {
    matches!(
        name,
        SPAWN_AGENT_TOOL
            | LIST_AGENTS_TOOL
            | GET_AGENT_TOOL
            | SEND_AGENT_MESSAGE_TOOL
            | INTERRUPT_AGENT_TOOL
            | WAIT_AGENT_TOOL
    )
}

fn explore_allows_native_function(name: &str) -> bool {
    matches!(
        name,
        UPDATE_GOAL_TOOL | REVIEW_DIFF_TOOL | SUBMIT_REVIEW_TOOL
    ) || is_lsp_function(name)
        || is_code_index_function(name)
        || is_skill_function(name)
}

#[derive(Default)]
struct ExternalRuntimeConfig {
    mcp: Option<McpConfig>,
    lsp: Option<LspConfig>,
    code_index: Option<CodeIndexConfig>,
    github: Option<GitHubConfig>,
}

impl Orchestrator {
    pub fn new(
        api_config: ApiConfig,
        agent_config: AgentConfig,
        event_tx: mpsc::Sender<OrchestratorEvent>,
        command_rx: mpsc::Receiver<OrchestratorCommand>,
    ) -> Result<Self, AppError> {
        let (snapshot_tx, _snapshot_rx) = watch::channel(UiSnapshot::default());
        Self::build(
            api_config,
            agent_config,
            event_tx,
            snapshot_tx,
            command_rx,
            UrgentControlHandle::default(),
            ExternalRuntimeConfig::default(),
        )
    }

    /// Creates an orchestrator together with its coalescing UI state stream.
    /// The returned receiver always contains a complete current snapshot, so
    /// dropped diagnostic events cannot leave the terminal in a stale state.
    pub fn with_snapshot(
        api_config: ApiConfig,
        agent_config: AgentConfig,
        event_tx: mpsc::Sender<OrchestratorEvent>,
        command_rx: mpsc::Receiver<OrchestratorCommand>,
    ) -> Result<(Self, watch::Receiver<UiSnapshot>), AppError> {
        let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::default());
        let orchestrator = Self::build(
            api_config,
            agent_config,
            event_tx,
            snapshot_tx,
            command_rx,
            UrgentControlHandle::default(),
            ExternalRuntimeConfig::default(),
        )?;
        Ok((orchestrator, snapshot_rx))
    }

    /// Full runtime wiring used by the TUI: normal commands stay in the
    /// bounded queue while urgent controls use a separate coalescing plane.
    pub fn with_runtime(
        api_config: ApiConfig,
        agent_config: AgentConfig,
        event_tx: mpsc::Sender<OrchestratorEvent>,
        command_rx: mpsc::Receiver<OrchestratorCommand>,
    ) -> Result<(Self, watch::Receiver<UiSnapshot>, UrgentControlHandle), AppError> {
        let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::default());
        let urgent_control = UrgentControlHandle::default();
        let orchestrator = Self::build(
            api_config,
            agent_config,
            event_tx,
            snapshot_tx,
            command_rx,
            urgent_control.clone(),
            ExternalRuntimeConfig::default(),
        )?;
        Ok((orchestrator, snapshot_rx, urgent_control))
    }

    /// Full TUI runtime with native MCP tools enabled from trusted global
    /// configuration. Tests and embedders can continue using `with_runtime`
    /// without starting external processes or network connections.
    #[allow(clippy::too_many_arguments)]
    pub fn with_runtime_and_mcp(
        api_config: ApiConfig,
        agent_config: AgentConfig,
        mcp_config: McpConfig,
        lsp_config: LspConfig,
        code_index_config: CodeIndexConfig,
        github_config: GitHubConfig,
        event_tx: mpsc::Sender<OrchestratorEvent>,
        command_rx: mpsc::Receiver<OrchestratorCommand>,
    ) -> Result<(Self, watch::Receiver<UiSnapshot>, UrgentControlHandle), AppError> {
        let (snapshot_tx, snapshot_rx) = watch::channel(UiSnapshot::default());
        let urgent_control = UrgentControlHandle::default();
        let orchestrator = Self::build(
            api_config,
            agent_config,
            event_tx,
            snapshot_tx,
            command_rx,
            urgent_control.clone(),
            ExternalRuntimeConfig {
                mcp: Some(mcp_config),
                lsp: Some(lsp_config),
                code_index: Some(code_index_config),
                github: Some(github_config),
            },
        )?;
        Ok((orchestrator, snapshot_rx, urgent_control))
    }

    fn build(
        api_config: ApiConfig,
        agent_config: AgentConfig,
        event_tx: mpsc::Sender<OrchestratorEvent>,
        snapshot_tx: watch::Sender<UiSnapshot>,
        command_rx: mpsc::Receiver<OrchestratorCommand>,
        urgent_control: UrgentControlHandle,
        external_runtime: ExternalRuntimeConfig,
    ) -> Result<Self, AppError> {
        let deployment = api_config.deployment.clone();
        let provider = api_config.provider;
        let base_max_output_tokens = api_config.max_output_tokens;
        let base_reasoning_effort = api_config.reasoning_effort;
        let temperature = api_config.temperature;
        let context_management = api_config.context_management();
        let mut pricing = api_config.pricing.clone();
        let pricing_catalog_url = api_config.pricing_catalog_url.clone();
        let pricing_path = agent_config
            .session_dir
            .parent()
            .unwrap_or(&agent_config.session_dir)
            .join(format!("pricing-overrides-{}.toml", provider.id()));
        let pricing_cache_path =
            pricing_path.with_file_name(format!("pricing-catalog-{}.json", provider.id()));
        if pricing_catalog_url.is_some()
            && let Err(error) =
                load_remote_pricing_cache(&pricing_cache_path, provider.id(), &mut pricing)
        {
            tracing::warn!(%error, "cached remote pricing catalog was ignored");
        }
        let base_pricing = pricing.clone();
        let (pricing_overrides, pricing_load_warning) =
            match PricingOverrideStore::load(pricing_path.clone()) {
                Ok(store) => (store, None),
                Err(error) => {
                    tracing::warn!(%error, "pricing overrides were ignored");
                    (
                        PricingOverrideStore::empty(pricing_path),
                        Some(format!("Pricing overrides were ignored: {error}")),
                    )
                }
            };
        for override_rate in pricing_overrides.rates() {
            pricing.upsert(override_rate.clone());
        }
        let deployment_choices = Arc::from(api_config.deployment_choices.clone());
        let (side_result_tx, side_result_rx) = mpsc::channel(8);
        let privacy = PrivacyShield::load(
            &agent_config.workspace_root,
            Some(agent_config.privacy_user_rules_file.clone()),
        )?;
        let instructions = InstructionCatalog::load_with_privacy(
            agent_config.workspace_root.clone(),
            &agent_config.instructions_file,
            &agent_config.instructions,
            agent_config.project_instructions.clone(),
            Some(privacy.clone()),
        );
        let skills = SkillCatalog::load(
            agent_config.workspace_root.clone(),
            agent_config.skills.clone(),
            Some(privacy.clone()),
        );
        let automation = Arc::new(Mutex::new(AutomationCatalog::load(
            agent_config.workspace_root.clone(),
        )));
        let integration_root = agent_config
            .skills
            .user_dir
            .parent()
            .unwrap_or(&agent_config.skills.user_dir)
            .to_path_buf();
        let plugin_storage_root = agent_config
            .session_dir
            .parent()
            .unwrap_or(&agent_config.session_dir)
            .join("plugins");
        let plugins = PluginManager::open(
            plugin_storage_root,
            integration_root,
            api_config.request_timeout,
        )?;
        let subagents = SubagentCoordinator::new_with_mcp(
            api_config.clone(),
            agent_config.clone(),
            snapshot_tx.clone(),
            external_runtime.mcp.clone(),
        )?;
        let client = Arc::new(ResponsesClient::new(api_config)?);
        let attachment_store = AttachmentStore::open(
            agent_config
                .session_dir
                .parent()
                .unwrap_or(&agent_config.session_dir)
                .join("attachments"),
        )?;
        let exec_options = ExecOptions::new(agent_config.exec_timeout, DEFAULT_MAX_OUTPUT_BYTES)
            .with_confirmation_mode(agent_config.shell.confirmation_mode)
            .with_strict_allowlist_entries(agent_config.shell.direct_exec_allowlist.clone());
        let tool_runner = Arc::new(ToolRunner::with_exec_options_and_privacy(
            &agent_config.workspace_root,
            exec_options,
            privacy.clone(),
        )?);
        let checkpoint_store =
            CheckpointStore::open(&agent_config.workspace_root, agent_config.exec_timeout)?;
        let session_store = SessionStore::open_at(
            agent_config.session_dir.clone(),
            &agent_config.workspace_root,
        )?;
        let mcp = external_runtime.mcp.map(McpManager::new).transpose()?;
        let lsp = external_runtime
            .lsp
            .map(|config| {
                LspManager::new_with_privacy(config, &agent_config.workspace_root, privacy.clone())
            })
            .transpose()?;
        let managed_connections =
            crate::managed_connections::ManagedConnectionStore::from_skills_dir(
                &agent_config.skills.user_dir,
            );
        let index_storage_root = agent_config
            .session_dir
            .parent()
            .unwrap_or(&agent_config.session_dir)
            .to_path_buf();
        let code_index = external_runtime
            .code_index
            .map(|config| {
                CodeIndexManager::new_with_privacy(
                    config,
                    &agent_config.workspace_root,
                    &index_storage_root,
                    privacy.clone(),
                )
            })
            .transpose()?;
        let github = external_runtime
            .github
            .map(|config| GitHubManager::new(config, &agent_config.workspace_root))
            .transpose()?;

        let skill_snapshot = skills.snapshot();
        let plugin_snapshot = plugins.snapshot();
        let max_context_budget = agent_config.max_context_budget;
        let default_context_budget = agent_config.context_budget;
        let mut state = AgentState::new();
        state.session_context_budget = Some(default_context_budget);
        let github_snapshot = github
            .as_ref()
            .map_or_else(GitHubSnapshot::default, GitHubManager::snapshot);
        snapshot_tx.send_modify(|snapshot| {
            snapshot.privacy = privacy.snapshot().unwrap_or_default();
            snapshot.skills = skill_snapshot;
            snapshot.plugins = plugin_snapshot;
            snapshot.max_context_budget = max_context_budget;
            snapshot.github = github_snapshot;
            if let Some(warning) = pricing_load_warning {
                snapshot.status = warning;
            }
        });

        Ok(Self {
            client,
            attachment_store,
            tool_runner,
            subagents,
            mcp,
            lsp,
            managed_connections,
            code_index,
            github,
            privacy,
            session_shell_permissions: SessionShellPermissions::default(),
            agent_config,
            default_context_budget,
            instructions,
            skills,
            automation,
            plugins,
            provider,
            deployment,
            base_max_output_tokens,
            base_reasoning_effort,
            temperature,
            context_management,
            pricing,
            base_pricing,
            pricing_overrides,
            pricing_catalog_url,
            pricing_cache_path,
            deployment_choices,
            state,
            event_tx,
            snapshot_tx,
            command_rx,
            urgent_control,
            side_result_tx,
            side_result_rx,
            side_cancel: None,
            next_side_generation: 1,
            queue_dispatch_request: None,
            next_turn_id: 1,
            next_action_id: 1,
            next_continuation_id: 1,
            next_plan_review_id: 1,
            active_review: None,
            conversation_epoch: 1,
            last_whip: None,
            penalty_responses_remaining: 0,
            whip_retry_note_pending: None,
            pause_resume_note_pending: None,
            retryable_turn: None,
            checkpoint_store,
            pending_checkpoint: None,
            session_store,
            current_session_id: None,
            last_session_persist_revision: 0,
            last_session_persist_at: Instant::now(),
        })
    }

    pub async fn run(mut self) {
        self.refresh_pricing_catalog().await;
        self.subagents.start().await;
        self.run_inner().await;
        // A side response can win the race with shutdown after the final UI
        // command. Persist every result that is already available before the
        // cancellation token is dropped; unfinished requests remain cancelled
        // and are never replayed after restart.
        self.drain_ready_side_results().await;
        self.cancel_side_task();
        self.subagents.shutdown().await;
        if let Some(mcp) = &mut self.mcp {
            mcp.shutdown().await;
        }
        if let Some(lsp) = &mut self.lsp {
            lsp.shutdown().await;
        }
        if let Some(code_index) = &mut self.code_index {
            code_index.shutdown().await;
        }
    }

    async fn refresh_pricing_catalog(&mut self) {
        let Some(url) = self.pricing_catalog_url.as_deref() else {
            return;
        };
        let timeout = self
            .agent_config
            .exec_timeout
            .min(Duration::from_secs(5))
            .max(Duration::from_secs(2));
        match refresh_remote_pricing(url, &self.pricing_cache_path, self.provider.id(), timeout)
            .await
        {
            Ok((catalog, imported)) => {
                self.base_pricing.merge(catalog);
                self.pricing.clone_from(&self.base_pricing);
                for pricing in self.pricing_overrides.rates() {
                    self.pricing.upsert(pricing.clone());
                }
                tracing::info!(
                    provider = self.provider.id(),
                    imported,
                    "pricing catalog refreshed"
                );
            }
            Err(error) => {
                tracing::warn!(%error, "remote pricing refresh failed; retained verified and cached rates");
            }
        }
    }

    async fn run_inner(&mut self) {
        self.publish_instruction_snapshot(None);
        self.publish_skills_snapshot(None);
        self.publish_automation_snapshot(None);
        let startup_hooks = self
            .run_hook_event(
                HookEvent::SessionStart,
                None,
                serde_json::json!({
                    "event": "session_start",
                    "workspace": self.agent_config.workspace_root,
                    "session_id": self.current_session_id,
                }),
                &CancellationToken::new(),
            )
            .await;
        self.publish_hook_notes(&startup_hooks);
        if let Some(mcp) = &mut self.mcp
            && let Err(error) = mcp.start().await
        {
            let _ = self
                .emit(OrchestratorEvent::FatalError {
                    message: format!("Required MCP startup failed: {error}"),
                })
                .await;
            return;
        }
        if !self.publish_mcp_servers().await {
            return;
        }
        if let Some(lsp) = &mut self.lsp
            && let Err(error) = lsp.start().await
        {
            let _ = self
                .emit(OrchestratorEvent::FatalError {
                    message: format!("Required LSP startup failed: {error}"),
                })
                .await;
            return;
        }
        self.publish_lsp_snapshot(Some("LSP discovery complete"));
        if let Some(code_index) = &mut self.code_index {
            code_index.start().await;
        }
        self.publish_code_index_snapshot(Some("Repository index initialized"));
        if !self.publish_sessions(String::new(), false).await
            || !self
                .emit(OrchestratorEvent::RuntimeSettingsUpdated {
                    deployment: self.deployment.clone(),
                    reasoning_effort: self.base_reasoning_effort,
                    context_budget: self.agent_config.context_budget,
                })
                .await
        {
            return;
        }
        if !self.publish_checkpoints().await {
            return;
        }
        if !self.set_phase(None, AgentPhase::Idle).await {
            return;
        }

        loop {
            if let Some(allow_manual_first) = self.queue_dispatch_request.take()
                && !self.dispatch_queued_follow_ups(allow_manual_first).await
            {
                return;
            }
            let urgent = self.urgent_control.clone();
            let command = tokio::select! {
                biased;
                _ = urgent.notified() => {
                    if !self.handle_idle_urgent().await {
                        return;
                    }
                    continue;
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                    continue;
                }
                command = self.command_rx.recv() => command,
            };
            let Some(command) = command else {
                return;
            };
            let Some(command) = self.handle_side_chat_command(command).await else {
                continue;
            };
            let Some(command) = self.handle_follow_up_command(command).await else {
                continue;
            };
            let Some(command) = self.handle_subagent_command(command).await else {
                continue;
            };
            match command {
                OrchestratorCommand::Submit {
                    prompt,
                    attachments,
                    scope,
                } => {
                    let (current_scope, idle) = {
                        let current = self.snapshot_tx.borrow();
                        (
                            CommandScope {
                                conversation_epoch: current.conversation_epoch,
                                phase_revision: current.phase_revision,
                            },
                            matches!(current.phase, AgentPhase::Idle | AgentPhase::Error { .. }),
                        )
                    };
                    if scope != current_scope || !idle {
                        let _ = self
                            .emit(OrchestratorEvent::BusyRejected {
                                turn_id: self.next_turn_id,
                                message: "Submit scope is stale; the prompt was not queued."
                                    .to_owned(),
                            })
                            .await;
                        continue;
                    }
                    if !self.submit_prompt(prompt, attachments, None).await {
                        return;
                    }
                    if !self.dispatch_queued_follow_ups(false).await {
                        return;
                    }
                }
                OrchestratorCommand::RetryTurn { turn_id }
                    if self.retryable_turn == Some(turn_id) =>
                {
                    let queued_id = self
                        .state
                        .follow_ups
                        .dispatching_for_turn(turn_id)
                        .map(|item| item.id);
                    let resumed_pause = self.state.resume_paused_turn(turn_id) > 0;
                    self.state.begin_turn(turn_id);
                    if resumed_pause {
                        self.pause_resume_note_pending = Some(turn_id);
                        if let Some(store) = &mut self.checkpoint_store {
                            match store
                                .begin(
                                    "Resume explicitly paused turn",
                                    &self.state,
                                    self.current_session_id.as_ref().map(ToString::to_string),
                                )
                                .await
                            {
                                Ok(checkpoint) => self.pending_checkpoint = Some(checkpoint),
                                Err(error) => {
                                    self.state.mark_turn_paused(turn_id);
                                    self.state.finish_turn(turn_id);
                                    self.pause_resume_note_pending = None;
                                    let _ = self
                                        .emit(OrchestratorEvent::RecoverableError {
                                            turn_id: Some(turn_id),
                                            message: format!(
                                                "Paused turn was not resumed because its Git checkpoint could not be created: {error}"
                                            ),
                                        })
                                        .await;
                                    continue;
                                }
                            }
                        }
                        if !self.emit_history_durable().await {
                            return;
                        }
                    }
                    self.retryable_turn = None;
                    if !self.drive_turn(turn_id).await {
                        return;
                    }
                    if let Some(id) = queued_id {
                        self.reconcile_dispatched_follow_up(id, turn_id).await;
                    }
                    if !self.dispatch_queued_follow_ups(false).await {
                        return;
                    }
                }
                OrchestratorCommand::AbortTurn { turn_id }
                    if self.retryable_turn == Some(turn_id) =>
                {
                    self.retryable_turn = None;
                    self.state.finish_turn(turn_id);
                    if self.whip_retry_note_pending == Some(turn_id) {
                        self.whip_retry_note_pending = None;
                    }
                    if self.pause_resume_note_pending == Some(turn_id) {
                        self.pause_resume_note_pending = None;
                    }
                    let was_paused = self.state.paused_turn_id == Some(turn_id);
                    self.state.clear_turn_metrics(turn_id);
                    if was_paused {
                        self.state.mark_turn_cancelled(turn_id);
                    } else {
                        self.state.mark_turn_failed(turn_id);
                    }
                    self.state
                        .follow_ups
                        .fail_pending_steers_for_turn(turn_id, UiNotice::FollowUpInterrupted);
                    if let Some(id) = self
                        .state
                        .follow_ups
                        .dispatching_for_turn(turn_id)
                        .map(|item| item.id)
                    {
                        let _ = self
                            .state
                            .follow_ups
                            .mark_failed(id, UiNotice::FollowUpInterrupted);
                        self.publish_follow_ups_status(UiNotice::FollowUpInterrupted);
                    }
                    if !self.finalize_checkpoint().await
                        || !self.emit_history_durable().await
                        || !self.set_phase(Some(turn_id), AgentPhase::Idle).await
                        || !self.emit(OrchestratorEvent::Done { turn_id }).await
                        || !self.publish_sessions(String::new(), false).await
                    {
                        return;
                    }
                    if !self.dispatch_queued_follow_ups(false).await {
                        return;
                    }
                }
                OrchestratorCommand::Reset => {
                    if self.finalize_checkpoint().await {
                        self.reset_state().await;
                    } else {
                        return;
                    }
                }
                OrchestratorCommand::Rewind {
                    checkpoint_id,
                    scope,
                } => {
                    let current_scope = {
                        let current = self.snapshot_tx.borrow();
                        CommandScope {
                            conversation_epoch: current.conversation_epoch,
                            phase_revision: current.phase_revision,
                        }
                    };
                    if scope == current_scope
                        && self.retryable_turn.is_none()
                        && !self.rewind_checkpoint(checkpoint_id).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::RefreshSessions {
                    query,
                    include_archived,
                } => {
                    if !self.publish_sessions(query, include_archived).await {
                        return;
                    }
                }
                OrchestratorCommand::NewSession { scope } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self.start_new_session().await
                    {
                        return;
                    }
                }
                OrchestratorCommand::ResumeSession {
                    session_id,
                    allow_workspace_mismatch,
                    scope,
                } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self
                            .resume_session(session_id, allow_workspace_mismatch)
                            .await
                    {
                        return;
                    }
                }
                OrchestratorCommand::ForkSession { session_id, scope } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self.fork_session(session_id).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::RenameSession {
                    session_id,
                    title,
                    scope,
                } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self.rename_session(session_id, title).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetSessionPinned {
                    session_id,
                    pinned,
                    scope,
                } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self.set_session_pinned(session_id, pinned).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetSessionArchived {
                    session_id,
                    archived,
                    scope,
                } => {
                    if self.accepts_session_navigation_scope(scope)
                        && !self.set_session_archived(session_id, archived).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::UpdateRuntimeSettings {
                    deployment,
                    reasoning_effort,
                    deep_thinking,
                    context_budget,
                    scope,
                } => {
                    if self.accepts_runtime_session_scope(scope)
                        && !self
                            .update_runtime_settings(
                                deployment,
                                reasoning_effort,
                                deep_thinking,
                                context_budget,
                            )
                            .await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetDeploymentPricing { pricing, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let pricing = pricing.as_user_override();
                        let deployment = pricing.deployment().to_owned();
                        match self.pricing_overrides.set(pricing.clone()) {
                            Ok(()) => {
                                self.pricing.upsert(pricing);
                                let usage = self.usage_snapshot();
                                if !self
                                    .emit(OrchestratorEvent::Usage { turn_id: 0, usage })
                                    .await
                                {
                                    return;
                                }
                                self.snapshot_tx.send_modify(|snapshot| {
                                    snapshot.status = format!(
                                        "Saved exact per-1M-token tariff for {deployment}; session cost recalculated"
                                    );
                                });
                            }
                            Err(error) => {
                                let _ = self
                                    .emit(OrchestratorEvent::RecoverableError {
                                        turn_id: None,
                                        message: format!(
                                            "deployment tariff was not saved: {error}"
                                        ),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                OrchestratorCommand::RemoveDeploymentPricing { deployment, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        match self.pricing_overrides.remove(&deployment) {
                            Ok(removed) => {
                                self.pricing.clone_from(&self.base_pricing);
                                for pricing in self.pricing_overrides.rates() {
                                    self.pricing.upsert(pricing.clone());
                                }
                                let usage = self.usage_snapshot();
                                if !self
                                    .emit(OrchestratorEvent::Usage { turn_id: 0, usage })
                                    .await
                                {
                                    return;
                                }
                                self.snapshot_tx.send_modify(|snapshot| {
                                    snapshot.status = if removed {
                                        format!("Removed local tariff override for {deployment}")
                                    } else {
                                        format!("No local tariff override exists for {deployment}")
                                    };
                                });
                            }
                            Err(error) => {
                                let _ = self
                                    .emit(OrchestratorEvent::RecoverableError {
                                        turn_id: None,
                                        message: format!(
                                            "deployment tariff override was not removed: {error}"
                                        ),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                OrchestratorCommand::GitHubRefresh { scope } => {
                    if self.accepts_idle_session_scope(scope) && !self.github_refresh().await {
                        return;
                    }
                }
                OrchestratorCommand::GitHubOpen { number, scope } => {
                    if self.accepts_idle_session_scope(scope) && !self.github_open(number).await {
                        return;
                    }
                }
                OrchestratorCommand::GitHubCheckout { number, scope } => {
                    if self.accepts_idle_session_scope(scope) && !self.github_checkout(number).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::GitHubCreateDraft { scope } => {
                    if self.accepts_idle_session_scope(scope) && !self.github_create_draft().await {
                        return;
                    }
                }
                OrchestratorCommand::SetPlanMode { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self.update_work_modes(|modes| modes.plan = enabled).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetExploreMode { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self
                            .update_work_modes(|modes| modes.explore = enabled)
                            .await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetReviewMode { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self.update_work_modes(|modes| modes.review = enabled).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetDeepThinkingMode { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self
                            .update_work_modes(|modes| modes.deep_thinking = enabled)
                            .await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetAutoApprovalPolicy { policy, scope } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self.update_auto_approval(policy).await
                    {
                        return;
                    }
                }
                OrchestratorCommand::SetGoal { objective, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        match self.state.work_modes.set_goal(objective) {
                            Ok(()) => {
                                if !self.work_modes_changed().await {
                                    return;
                                }
                            }
                            Err(error) => {
                                let _ = self
                                    .emit(OrchestratorEvent::RecoverableError {
                                        turn_id: None,
                                        message: error.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                OrchestratorCommand::ReloadProjectInstructions { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.reload_project_instructions(true).await;
                    }
                }
                OrchestratorCommand::SetProjectInstructionsEnabled { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.instructions.set_project_enabled(enabled);
                        self.publish_instruction_snapshot(Some(if enabled {
                            "Repository instructions enabled"
                        } else {
                            "Repository instructions disabled for this run"
                        }));
                    }
                }
                OrchestratorCommand::SetInstructionSourceEnabled { id, enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        match self.instructions.set_source_enabled(&id, enabled) {
                            Ok(()) => self.publish_instruction_snapshot(Some(if enabled {
                                "Instruction source enabled"
                            } else {
                                "Instruction source disabled for this run"
                            })),
                            Err(error) => {
                                let _ = self
                                    .emit(OrchestratorEvent::RecoverableError {
                                        turn_id: None,
                                        message: error.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                OrchestratorCommand::ReloadSkills { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.reload_skills(true).await;
                    }
                }
                OrchestratorCommand::SetSkillEnabled { id, enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        match self.skills.set_enabled(&id, enabled) {
                            Ok(()) => self.publish_skills_snapshot(Some(if enabled {
                                "Skill enabled for this run"
                            } else {
                                "Skill disabled for this run"
                            })),
                            Err(error) => {
                                let _ = self
                                    .emit(OrchestratorEvent::RecoverableError {
                                        turn_id: None,
                                        message: error.to_string(),
                                    })
                                    .await;
                            }
                        }
                    }
                }
                OrchestratorCommand::ReloadAutomation { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.reload_automation().await;
                    }
                }
                OrchestratorCommand::SetHookEnabled { id, enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.set_hook_enabled(&id, enabled);
                    }
                }
                OrchestratorCommand::RefreshPlugins { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.plugins.refresh_marketplaces().await;
                        self.publish_plugin_snapshot(Some("Plugin marketplaces refreshed"));
                    }
                }
                OrchestratorCommand::AddPluginMarketplace { source, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self
                            .plugins
                            .add_marketplace(source)
                            .map(|()| "Plugin marketplace added".to_owned());
                        self.finish_plugin_action(result, false).await;
                    }
                }
                OrchestratorCommand::RemovePluginMarketplace { source, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self
                            .plugins
                            .remove_marketplace(&source)
                            .map(|()| "Plugin marketplace removed".to_owned());
                        self.finish_plugin_action(result, false).await;
                    }
                }
                OrchestratorCommand::InstallLocalPlugin { package, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let path = PathBuf::from(package);
                        let path = if path.is_absolute() {
                            path
                        } else {
                            self.agent_config.workspace_root.join(path)
                        };
                        let result = self.plugins.install_local(&path).await;
                        self.finish_plugin_action(result, false).await;
                    }
                }
                OrchestratorCommand::InstallMarketplacePlugin { id, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self.plugins.install_marketplace(&id).await;
                        self.finish_plugin_action(result, false).await;
                    }
                }
                OrchestratorCommand::UpdatePlugin { id, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self.plugins.update(&id).await;
                        self.finish_plugin_action(result, true).await;
                    }
                }
                OrchestratorCommand::SetPluginEnabled { id, enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self.plugins.set_enabled(&id, enabled).map(|()| {
                            format!(
                                "Plugin {id} {}",
                                if enabled { "enabled" } else { "disabled" }
                            )
                        });
                        self.finish_plugin_action(result, true).await;
                    }
                }
                OrchestratorCommand::RemovePlugin { id, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self
                            .plugins
                            .remove(&id)
                            .map(|()| format!("Plugin {id} removed"));
                        self.finish_plugin_action(result, true).await;
                    }
                }
                OrchestratorCommand::McpConnect { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_connect(&server).await;
                    }
                }
                OrchestratorCommand::McpDisconnect { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        if let Some(mcp) = &mut self.mcp {
                            mcp.disconnect(&server).await;
                        }
                        if !self.publish_mcp_servers().await {
                            return;
                        }
                    }
                }
                OrchestratorCommand::McpSetEnabled {
                    server,
                    enabled,
                    scope,
                } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_set_enabled(&server, enabled).await;
                    }
                }
                OrchestratorCommand::McpAddServer { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_add_server(server).await;
                    }
                }
                OrchestratorCommand::SetSubagentMcpAccess { enabled, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self.subagents.set_mcp_enabled(enabled).await;
                        if let Err(error) = result {
                            self.snapshot_tx.send_modify(|snapshot| {
                                snapshot.status =
                                    format!("Sub-agent MCP access was not changed: {error}");
                            });
                        }
                    }
                }
                OrchestratorCommand::McpBeginOAuth { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_begin_oauth(&server).await;
                    }
                }
                OrchestratorCommand::McpPollOAuth { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_poll_oauth(&server).await;
                    }
                }
                OrchestratorCommand::McpForgetOAuth { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_mcp_forget_oauth(&server).await;
                    }
                }
                OrchestratorCommand::LspConnect { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_lsp_connect(&server).await;
                    }
                }
                OrchestratorCommand::LspDisconnect { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        if let Some(lsp) = &mut self.lsp {
                            lsp.disconnect(&server).await;
                        }
                        self.publish_lsp_snapshot(Some("Language server stopped"));
                    }
                }
                OrchestratorCommand::LspSetEnabled {
                    server,
                    enabled,
                    scope,
                } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_lsp_set_enabled(&server, enabled).await;
                    }
                }
                OrchestratorCommand::LspAddServer { server, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.handle_lsp_add_server(server);
                    }
                }
                OrchestratorCommand::LspRefresh { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        self.publish_lsp_snapshot(None);
                    }
                }
                OrchestratorCommand::CodeIndexRefresh { force, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self
                            .code_index
                            .as_mut()
                            .ok_or(crate::code_index::CodeIndexError::Disabled)
                            .and_then(|index| index.start_refresh(force));
                        let status = match result {
                            Ok(()) => if force {
                                "Repository index rebuild started"
                            } else {
                                "Repository index refresh started"
                            }
                            .to_owned(),
                            Err(error) => format!("Repository index did not start: {error}"),
                        };
                        self.publish_code_index_snapshot(Some(&status));
                    }
                }
                OrchestratorCommand::CodeIndexCancel { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = self
                            .code_index
                            .as_ref()
                            .ok_or(crate::code_index::CodeIndexError::Disabled)
                            .and_then(CodeIndexManager::cancel_refresh);
                        let status = result.map_or_else(
                            |error| format!("Repository index was not cancelled: {error}"),
                            |()| "Cancelling repository index refresh".to_owned(),
                        );
                        self.publish_code_index_snapshot(Some(&status));
                    }
                }
                OrchestratorCommand::CodeIndexPoll { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        if let Some(index) = &mut self.code_index {
                            index.poll().await;
                        }
                        self.publish_code_index_snapshot(None);
                    }
                }
                OrchestratorCommand::CodeIndexSearch {
                    query,
                    path,
                    top,
                    scope,
                } => {
                    if self.accepts_idle_session_scope(scope) {
                        let result = match &mut self.code_index {
                            Some(index) => {
                                index
                                    .search(&query, path.as_deref(), top, &CancellationToken::new())
                                    .await
                            }
                            None => Err(crate::code_index::CodeIndexError::Disabled),
                        };
                        match result {
                            Ok(hits) => {
                                self.snapshot_tx.send_modify(|snapshot| {
                                    snapshot.code_index_hits = Arc::from(hits);
                                    snapshot.status =
                                        format!("Repository search completed for {:?}", query);
                                });
                            }
                            Err(error) => self.publish_code_index_snapshot(Some(&format!(
                                "Repository search failed: {error}"
                            ))),
                        }
                    }
                }
                OrchestratorCommand::ReloadPrivacy { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let status = match self.privacy.reload() {
                            Ok(snapshot) => {
                                self.snapshot_tx.send_modify(|ui| ui.privacy = snapshot);
                                self.instructions.reload();
                                self.publish_instruction_snapshot(None);
                                if let Some(lsp) = &self.lsp {
                                    lsp.privacy_reloaded();
                                }
                                let index_status = self.code_index.as_mut().map(|index| {
                                    index.privacy_reloaded().map_or_else(
                                        |error| format!("; index invalidation failed: {error}"),
                                        |changed| {
                                            if changed {
                                                "; stale repository index invalidated".to_owned()
                                            } else {
                                                String::new()
                                            }
                                        },
                                    )
                                });
                                format!(
                                    "Privacy Shield rules reloaded{}",
                                    index_status.unwrap_or_default()
                                )
                            }
                            Err(error) => format!(
                                "Privacy Shield reload failed; the previous policy remains active: {error}"
                            ),
                        };
                        self.snapshot_tx.send_modify(|ui| ui.status = status);
                    }
                }
                OrchestratorCommand::RevokeSessionShellGrant { grant_id, scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let status = if self.session_shell_permissions.revoke(grant_id) {
                            "Exact shell permission revoked"
                        } else {
                            "Shell permission was already absent"
                        };
                        self.publish_shell_permissions(status);
                    }
                }
                OrchestratorCommand::ClearSessionShellGrants { scope } => {
                    if self.accepts_idle_session_scope(scope) {
                        let status = if self.session_shell_permissions.clear() {
                            "All session shell permissions revoked"
                        } else {
                            "No session shell permissions to revoke"
                        };
                        self.publish_shell_permissions(status);
                    }
                }
                OrchestratorCommand::DecideReviewFinding {
                    report_id,
                    revision,
                    finding_id,
                    decision,
                    scope,
                } => {
                    if self.accepts_idle_session_scope(scope)
                        && !self
                            .decide_review_finding(report_id, revision, finding_id, decision)
                            .await
                    {
                        return;
                    }
                }
                OrchestratorCommand::Shutdown => {
                    let _ = self.prepare_session_transition().await;
                    return;
                }
                OrchestratorCommand::Confirm { .. }
                | OrchestratorCommand::DecidePlan { .. }
                | OrchestratorCommand::DecidePatch { .. }
                | OrchestratorCommand::SpawnSubagent { .. }
                | OrchestratorCommand::ReloadSubagentProfiles
                | OrchestratorCommand::MessageSubagent { .. }
                | OrchestratorCommand::CancelSubagent { .. }
                | OrchestratorCommand::ResumeSubagent { .. }
                | OrchestratorCommand::AbandonSubagentRecovery { .. }
                | OrchestratorCommand::DecideSubagentCommand { .. }
                | OrchestratorCommand::DecideSubagentBudget { .. }
                | OrchestratorCommand::OpenSubagentReview { .. }
                | OrchestratorCommand::DecideSubagentFile { .. }
                | OrchestratorCommand::AskSideQuestion { .. }
                | OrchestratorCommand::CancelSideQuestion { .. }
                | OrchestratorCommand::EnqueueFollowUp { .. }
                | OrchestratorCommand::EditFollowUp { .. }
                | OrchestratorCommand::CancelFollowUp { .. }
                | OrchestratorCommand::RetryFollowUp { .. }
                | OrchestratorCommand::DispatchFollowUpQueue { .. }
                | OrchestratorCommand::Whip { .. }
                | OrchestratorCommand::Interrupt { .. }
                | OrchestratorCommand::ContinueToolLoop { .. }
                | OrchestratorCommand::RetryTurn { .. }
                | OrchestratorCommand::AbortTurn { .. } => {
                    // Stale control commands are intentionally ignored while idle.
                }
            }
        }
    }

    async fn handle_side_chat_command(
        &mut self,
        command: OrchestratorCommand,
    ) -> Option<OrchestratorCommand> {
        self.drain_ready_side_results().await;
        match command {
            OrchestratorCommand::AskSideQuestion {
                question,
                deployment,
                reasoning_effort,
                scope,
            } => {
                let current_scope = {
                    let snapshot = self.snapshot_tx.borrow();
                    CommandScope {
                        conversation_epoch: snapshot.conversation_epoch,
                        phase_revision: snapshot.phase_revision,
                    }
                };
                if scope != current_scope {
                    self.publish_side_chat_status(UiNotice::StaleUiAction);
                    return None;
                }
                if let Err(error) = validate_question(&question) {
                    self.publish_side_chat_status(UiNotice::external(error.to_string()));
                    return None;
                }
                if !self
                    .deployment_choices
                    .iter()
                    .any(|choice| choice == &deployment)
                {
                    self.publish_side_chat_status(UiNotice::TrustedDeploymentRequired);
                    return None;
                }
                let mut request = match self.build_side_question_request(
                    &question,
                    &deployment,
                    reasoning_effort,
                ) {
                    Ok(request) => request,
                    Err(error) => {
                        self.publish_side_chat_status(UiNotice::external(error.to_string()));
                        return None;
                    }
                };
                if let Err(error) = self
                    .attachment_store
                    .hydrate_request(&mut request, self.client.capabilities())
                {
                    self.publish_side_chat_status(UiNotice::external(error.to_string()));
                    return None;
                }
                if !self.ensure_current_session(&question).await {
                    return None;
                }
                let exchange = match self.state.side_chat.start(
                    question,
                    self.state.history_revision,
                    deployment,
                    reasoning_effort,
                ) {
                    Ok(exchange) => exchange,
                    Err(error) => {
                        self.publish_side_chat_status(UiNotice::external(error.to_string()));
                        return None;
                    }
                };
                self.publish_side_chat_status(UiNotice::SideQuestionRunning);
                if !self.persist_current_session(true).await {
                    let _ = self
                        .state
                        .side_chat
                        .fail(exchange.id, UiNotice::PersistenceBlocked);
                    self.publish_side_chat_status(UiNotice::PersistenceBlocked);
                    return None;
                }
                let cancel = CancellationToken::new();
                let generation = self.next_side_generation;
                self.next_side_generation = generation.saturating_add(1);
                self.side_cancel = Some((exchange.id, generation, cancel.clone()));
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.side_task_generation = generation;
                });
                let client = Arc::clone(&self.client);
                let result_tx = self.side_result_tx.clone();
                let snapshot_tx = self.snapshot_tx.clone();
                let conversation_epoch = self.conversation_epoch;
                tokio::spawn(async move {
                    let panic_exchange = exchange.clone();
                    let future = run_side_question(client, request, cancel, exchange);
                    let exchange = AssertUnwindSafe(future)
                        .catch_unwind()
                        .await
                        .unwrap_or_else(move |_| SideExchange {
                            answer: String::new(),
                            status: SideExchangeStatus::Failed,
                            notice: UiNotice::DependencyFailure,
                            completed_at: Some(chrono::Utc::now()),
                            ..panic_exchange
                        });
                    snapshot_tx.send_modify(|snapshot| {
                        if snapshot.conversation_epoch == conversation_epoch
                            && snapshot.side_task_generation == generation
                        {
                            snapshot.side_chat.apply_preview(exchange.clone());
                        }
                    });
                    let _ = result_tx
                        .send(SideTaskResult {
                            conversation_epoch,
                            generation,
                            exchange,
                        })
                        .await;
                });
                None
            }
            OrchestratorCommand::CancelSideQuestion { request_id, scope } => {
                let current_scope = {
                    let snapshot = self.snapshot_tx.borrow();
                    CommandScope {
                        conversation_epoch: snapshot.conversation_epoch,
                        phase_revision: snapshot.phase_revision,
                    }
                };
                if scope != current_scope {
                    self.publish_side_chat_status(UiNotice::StaleUiAction);
                    return None;
                }
                if self
                    .side_cancel
                    .as_ref()
                    .is_some_and(|(active, _, _)| *active == request_id)
                {
                    if let Some((_, _, cancel)) = self.side_cancel.take() {
                        cancel.cancel();
                    }
                    self.snapshot_tx.send_modify(|snapshot| {
                        snapshot.side_task_generation = 0;
                    });
                }
                match self.state.side_chat.cancel(request_id) {
                    Ok(()) => {
                        self.publish_side_chat_status(UiNotice::SideQuestionCancelled);
                        let _ = self.persist_current_session(true).await;
                    }
                    Err(error) => {
                        self.publish_side_chat_status(UiNotice::external(error.to_string()))
                    }
                }
                None
            }
            other => Some(other),
        }
    }

    fn build_side_question_request(
        &self,
        question: &str,
        deployment: &str,
        reasoning_effort: ReasoningEffort,
    ) -> Result<ResponsesRequest, crate::agent::ContextBudgetExceeded> {
        let mut instructions = self.effective_instructions();
        instructions.push_str("\n\n");
        instructions.push_str(SIDE_QUESTION_INSTRUCTIONS);
        let context_budget = self.request_context_budget_for(&instructions);
        let mut input = self.state.checked_stateless_replay_input(context_budget)?;
        input.push(serde_json::json!({"role": "user", "content": question}));
        Ok(ResponsesRequest::stateless_replay(
            deployment,
            instructions,
            input,
            self.base_max_output_tokens
                .min(SIDE_QUESTION_MAX_OUTPUT_TOKENS),
        )
        .with_reasoning(reasoning_effort)
        .with_temperature(self.temperature)
        .with_include(Vec::new()))
    }

    async fn drain_ready_side_results(&mut self) {
        loop {
            match self.side_result_rx.try_recv() {
                Ok(result) => self.finish_side_question(result).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
    }

    async fn finish_side_question(&mut self, result: SideTaskResult) {
        if result.conversation_epoch != self.conversation_epoch
            || !self
                .side_cancel
                .as_ref()
                .is_some_and(|(id, generation, _)| {
                    *id == result.exchange.id && *generation == result.generation
                })
        {
            return;
        }
        let exchange = result.exchange;
        let updated = match exchange.status {
            SideExchangeStatus::Completed => self.state.side_chat.complete(
                exchange.id,
                exchange.answer.clone(),
                exchange.input_tokens,
                exchange.cached_input_tokens,
                exchange.output_tokens,
                exchange.total_tokens,
            ),
            SideExchangeStatus::Failed => self
                .state
                .side_chat
                .fail(exchange.id, exchange.notice.clone()),
            SideExchangeStatus::Cancelled => self.state.side_chat.cancel(exchange.id),
            SideExchangeStatus::Running => return,
        };
        if updated.is_err() {
            return;
        }
        if exchange.total_tokens > 0 {
            self.state.billing_usage.record(
                &exchange.deployment,
                exchange.input_tokens,
                exchange.cached_input_tokens,
                exchange.output_tokens,
                exchange.total_tokens,
            );
        }
        if self
            .side_cancel
            .as_ref()
            .is_some_and(|(id, generation, _)| {
                *id == exchange.id && *generation == result.generation
            })
        {
            self.side_cancel = None;
        }
        let side_chat = self.state.side_chat.snapshot();
        let usage = self.usage_snapshot();
        let status = match exchange.status {
            SideExchangeStatus::Completed => "Side answer ready (provisional)",
            SideExchangeStatus::Failed => "Side question failed without changing the main task",
            SideExchangeStatus::Cancelled => "Side question cancelled",
            SideExchangeStatus::Running => "",
        }
        .to_owned();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.side_chat = side_chat.clone();
            snapshot.side_task_generation = 0;
            snapshot.usage = Some(usage.clone());
            snapshot.status.clone_from(&status);
        });
        let _ = self.persist_current_session(true).await;
    }

    fn publish_side_chat_status(&self, notice: UiNotice) {
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.side_chat = self.state.side_chat.snapshot();
            snapshot.status.clear();
            snapshot.notice = notice;
        });
    }

    fn cancel_side_task(&mut self) {
        if let Some((_, _, cancel)) = self.side_cancel.take() {
            cancel.cancel();
        }
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.side_task_generation = 0;
        });
    }

    async fn handle_follow_up_command(
        &mut self,
        command: OrchestratorCommand,
    ) -> Option<OrchestratorCommand> {
        let (current_scope, phase, active_turn_id) = {
            let snapshot = self.snapshot_tx.borrow();
            (
                CommandScope {
                    conversation_epoch: snapshot.conversation_epoch,
                    phase_revision: snapshot.phase_revision,
                },
                snapshot.phase.clone(),
                snapshot.active_turn_id,
            )
        };
        match command {
            OrchestratorCommand::EnqueueFollowUp { mode, text, scope } => {
                if scope != current_scope {
                    self.publish_follow_ups_status(UiNotice::StaleUiAction);
                    return None;
                }
                if let Err(error) = validate_follow_up(&text) {
                    self.publish_follow_ups_status(UiNotice::external(error.to_string()));
                    return None;
                }
                let target_turn = match mode {
                    FollowUpMode::Queue => None,
                    FollowUpMode::Steer if phase.is_busy() => active_turn_id,
                    FollowUpMode::Steer => {
                        self.publish_follow_ups_status(UiNotice::SteerRequiresActiveTurn);
                        return None;
                    }
                };
                if !self.ensure_current_session(&text).await {
                    return None;
                }
                let item = match self.state.follow_ups.enqueue(mode, text, target_turn) {
                    Ok(item) => item,
                    Err(error) => {
                        self.publish_follow_ups_status(UiNotice::external(error.to_string()));
                        return None;
                    }
                };
                self.publish_follow_ups_status(match mode {
                    FollowUpMode::Queue => UiNotice::FollowUpWaitingTurn,
                    FollowUpMode::Steer => UiNotice::FollowUpWaitingBoundary,
                });
                if !self.persist_current_session(true).await {
                    let _ = self
                        .state
                        .follow_ups
                        .mark_failed(item.id, UiNotice::PersistenceBlocked);
                    self.publish_follow_ups_status(UiNotice::PersistenceBlocked);
                    return None;
                }
                if mode == FollowUpMode::Queue && matches!(phase, AgentPhase::Idle) {
                    self.request_queue_dispatch(false);
                }
                None
            }
            OrchestratorCommand::EditFollowUp {
                id,
                revision,
                text,
                scope,
            } => {
                if scope != current_scope {
                    self.publish_follow_ups_status(UiNotice::StaleUiAction);
                    return None;
                }
                match self.state.follow_ups.edit(id, revision, text) {
                    Ok(()) => {
                        self.publish_follow_ups_status(UiNotice::FollowUpEditedPending);
                        let _ = self.persist_current_session(true).await;
                    }
                    Err(error) => {
                        self.publish_follow_ups_status(UiNotice::external(error.to_string()))
                    }
                }
                None
            }
            OrchestratorCommand::CancelFollowUp {
                id,
                revision,
                scope,
            } => {
                if scope != current_scope {
                    self.publish_follow_ups_status(UiNotice::StaleUiAction);
                    return None;
                }
                match self.state.follow_ups.cancel(id, revision) {
                    Ok(()) => {
                        self.publish_follow_ups_status(UiNotice::FollowUpCancelledBeforeDelivery);
                        let _ = self.persist_current_session(true).await;
                    }
                    Err(error) => {
                        self.publish_follow_ups_status(UiNotice::external(error.to_string()))
                    }
                }
                None
            }
            OrchestratorCommand::RetryFollowUp {
                id,
                revision,
                scope,
            } => {
                if scope != current_scope {
                    self.publish_follow_ups_status(UiNotice::StaleUiAction);
                    return None;
                }
                let mode = self
                    .state
                    .follow_ups
                    .snapshot()
                    .items
                    .iter()
                    .find(|item| item.id == id)
                    .map(|item| item.mode);
                let target = match mode {
                    Some(FollowUpMode::Steer) if phase.is_busy() => active_turn_id,
                    Some(FollowUpMode::Steer) => {
                        self.publish_follow_ups_status(UiNotice::SteerRequiresActiveTurn);
                        return None;
                    }
                    _ => None,
                };
                match self.state.follow_ups.retry(id, revision, target) {
                    Ok(()) => {
                        self.publish_follow_ups_status(match mode {
                            Some(FollowUpMode::Steer) => UiNotice::FollowUpRetrySteer,
                            _ => UiNotice::FollowUpRetryQueued,
                        });
                        let _ = self.persist_current_session(true).await;
                        if mode == Some(FollowUpMode::Queue) && matches!(phase, AgentPhase::Idle) {
                            self.request_queue_dispatch(true);
                        }
                    }
                    Err(error) => {
                        self.publish_follow_ups_status(UiNotice::external(error.to_string()))
                    }
                }
                None
            }
            OrchestratorCommand::DispatchFollowUpQueue { scope } => {
                if scope != current_scope || !matches!(phase, AgentPhase::Idle) {
                    self.publish_follow_ups_status(UiNotice::StaleUiAction);
                    return None;
                }
                self.request_queue_dispatch(true);
                None
            }
            other => Some(other),
        }
    }

    async fn dispatch_queued_follow_ups(&mut self, mut allow_manual_first: bool) -> bool {
        loop {
            let idle = matches!(self.snapshot_tx.borrow().phase, AgentPhase::Idle);
            if !idle {
                return true;
            }
            let item = if allow_manual_first {
                self.state.follow_ups.next_queue()
            } else {
                self.state.follow_ups.next_auto_queue()
            }
            .cloned();
            let Some(item) = item else {
                return true;
            };
            allow_manual_first = false;
            let was_pending_revision = item.revision;
            if !self
                .submit_prompt(item.text.clone(), Vec::new(), Some(item.id))
                .await
            {
                return false;
            }
            let still_pending = self
                .state
                .follow_ups
                .snapshot()
                .items
                .iter()
                .find(|candidate| candidate.id == item.id)
                .is_some_and(|candidate| {
                    candidate.status == FollowUpStatus::Pending
                        && candidate.revision == was_pending_revision
                });
            if still_pending {
                let _ = self
                    .state
                    .follow_ups
                    .mark_failed(item.id, UiNotice::FollowUpInterrupted);
                self.publish_follow_ups_status(UiNotice::FollowUpInterrupted);
                let _ = self.persist_current_session(true).await;
                return true;
            }
        }
    }

    async fn reconcile_dispatched_follow_up(&mut self, id: u64, turn_id: TurnId) {
        let delivered = self.state.history.iter().any(|entry| {
            entry.turn_id == turn_id
                && matches!(entry.kind, HistoryKind::User)
                && entry.is_committed()
        });
        if delivered {
            let _ = self.state.follow_ups.mark_delivered(id);
            self.publish_follow_ups_status(UiNotice::FollowUpDeliveredAsTurn {
                turn_id: Some(turn_id),
            });
        } else if self.retryable_turn != Some(turn_id) {
            let _ = self
                .state
                .follow_ups
                .mark_failed(id, UiNotice::FollowUpInterrupted);
            self.publish_follow_ups_status(UiNotice::FollowUpInterrupted);
        } else {
            self.publish_follow_ups_status(UiNotice::FollowUpEditedAfterFailure);
        }
        let _ = self.persist_current_session(true).await;
    }

    fn publish_follow_ups_status(&self, notice: UiNotice) {
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.follow_ups = self.state.follow_ups.snapshot();
            snapshot.status.clear();
            snapshot.notice = notice;
        });
    }

    fn request_queue_dispatch(&mut self, allow_manual_first: bool) {
        self.queue_dispatch_request =
            Some(self.queue_dispatch_request.is_some_and(|current| current) || allow_manual_first);
    }

    async fn handle_subagent_command(
        &mut self,
        command: OrchestratorCommand,
    ) -> Option<OrchestratorCommand> {
        let result = match command {
            OrchestratorCommand::SpawnSubagent {
                task,
                profile_id,
                dependencies,
                file_claims,
            } => self
                .subagents
                .spawn(SpawnSubagentRequest {
                    task,
                    profile_id,
                    session_id: self.current_session_id.as_ref().map(ToString::to_string),
                    deployment: self.deployment.clone(),
                    reasoning_effort: self.base_reasoning_effort,
                    instructions: self.effective_instructions(),
                    dependencies,
                    file_claims,
                })
                .await
                .map(|id| format!("Started {id}")),
            OrchestratorCommand::ReloadSubagentProfiles => {
                self.subagents.reload_profiles().await.map(|snapshot| {
                    format!(
                        "Reloaded {} agent profile(s); {} diagnostic(s)",
                        snapshot.profiles.len(),
                        snapshot.diagnostics.len()
                    )
                })
            }
            OrchestratorCommand::MessageSubagent {
                agent_id,
                expected_revision,
                message,
            } => self
                .subagents
                .send_message(agent_id, expected_revision, message)
                .map(|()| format!("Message sent to {agent_id}")),
            OrchestratorCommand::CancelSubagent {
                agent_id,
                expected_revision,
            } => self
                .subagents
                .cancel(agent_id, expected_revision)
                .map(|()| format!("Stopping {agent_id}")),
            OrchestratorCommand::ResumeSubagent {
                agent_id,
                expected_revision,
            } => {
                let instructions = self.effective_instructions();
                self.subagents
                    .resume(agent_id, expected_revision, instructions)
                    .await
                    .map(|()| format!("Resuming {agent_id} from its durable writer checkpoint"))
            }
            OrchestratorCommand::AbandonSubagentRecovery {
                agent_id,
                expected_revision,
            } => self
                .subagents
                .abandon_recovery(agent_id, expected_revision)
                .await
                .map(|()| format!("Abandoned recovery for {agent_id}")),
            OrchestratorCommand::DecideSubagentCommand {
                agent_id,
                expected_revision,
                action_id,
                approved,
            } => self
                .subagents
                .decide_command(agent_id, expected_revision, action_id, approved)
                .map(|()| {
                    if approved {
                        format!("Approved command for {agent_id}")
                    } else {
                        format!("Declined command for {agent_id}")
                    }
                }),
            OrchestratorCommand::DecideSubagentBudget {
                agent_id,
                expected_revision,
                approved,
            } => self
                .subagents
                .decide_budget(agent_id, expected_revision, approved)
                .map(|()| {
                    if approved {
                        format!("Raised token budget for {agent_id}")
                    } else {
                        format!("Stopped {agent_id} at its token budget")
                    }
                }),
            OrchestratorCommand::OpenSubagentReview {
                agent_id,
                expected_revision,
                change_digest,
                path,
                scope,
            } => {
                if !self.accepts_idle_session_scope(scope) {
                    Err(SubagentError::Unavailable(
                        "file review can only open while the main agent is idle".to_owned(),
                    ))
                } else {
                    match self.subagents.file_review(
                        agent_id,
                        expected_revision,
                        &change_digest,
                        &path,
                    ) {
                        Ok(review) if self.state.auto_approval.subagent_changes => {
                            let decision = if review.binary {
                                SubagentFileDecision::ApproveBinary
                            } else {
                                let hunk_count =
                                    review.review.as_ref().map_or(0, |patch| patch.hunks.len());
                                SubagentFileDecision::TextHunks(vec![true; hunk_count])
                            };
                            self.subagents
                                .decide_file(&review, decision, CancellationToken::new())
                                .await
                                .map(|()| {
                                    format!(
                                        "Auto-approved {path} from {agent_id} by session policy"
                                    )
                                })
                        }
                        Ok(review) => {
                            self.snapshot_tx.send_modify(|snapshot| {
                                snapshot.modal = Some(UiModal::SubagentPatchApproval {
                                    review: Arc::new(review),
                                });
                            });
                            Ok(format!("Reviewing {path} from {agent_id}"))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            OrchestratorCommand::DecideSubagentFile {
                review,
                decision,
                scope,
            } => {
                if !self.accepts_idle_session_scope(scope) {
                    Err(SubagentError::Unavailable(
                        "file integration can only run while the main agent is idle".to_owned(),
                    ))
                } else {
                    let path = review.path.clone();
                    let id = review.agent_id;
                    let result = self
                        .subagents
                        .decide_file(&review, decision, CancellationToken::new())
                        .await
                        .map(|()| format!("Reviewed {path} from {id}"));
                    self.snapshot_tx
                        .send_modify(|snapshot| snapshot.modal = None);
                    result
                }
            }
            other => return Some(other),
        };
        match result {
            Ok(message) => {
                self.snapshot_tx
                    .send_modify(|snapshot| snapshot.status = message);
            }
            Err(error) => {
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.status = format!("Sub-agent action failed: {error}");
                });
            }
        }
        None
    }

    async fn handle_idle_urgent(&mut self) -> bool {
        let mut reset = false;
        let mut shutdown = false;
        for signal in self.urgent_control.drain() {
            match signal.kind {
                UrgentControlKind::Shutdown => shutdown = true,
                UrgentControlKind::Reset => reset = true,
                UrgentControlKind::Whip { .. } | UrgentControlKind::Interrupt { .. } => {}
            }
        }
        if shutdown {
            let _ = self.prepare_session_transition().await;
            return false;
        }
        if reset {
            if !self.finalize_checkpoint().await {
                return false;
            }
            self.reset_state().await;
        }
        true
    }

    async fn submit_prompt(
        &mut self,
        prompt: String,
        attachment_paths: Vec<AttachmentSource>,
        queued_follow_up_id: Option<u64>,
    ) -> bool {
        let raw_prompt = prompt.trim().to_owned();
        if raw_prompt.is_empty() && attachment_paths.is_empty() {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: "Prompt must not be blank.".to_owned(),
                })
                .await;
        }
        // Custom commands and trusted user hooks are intentionally hot-reloaded
        // at the submission boundary. This keeps edits predictable: the text
        // visible in the editor is resolved against one catalog revision, and
        // that same revision supplies prompt hooks for the turn.
        self.reload_automation().await;
        let expansion = self
            .automation
            .lock()
            .map_err(|_| "automation catalog lock was poisoned".to_owned())
            .and_then(|catalog| {
                catalog
                    .expand_invocation(&raw_prompt)
                    .map_err(|error| error.to_string())
            });
        let prompt = match expansion {
            Ok(Some(expanded)) => {
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.status = "Expanded custom slash command".to_owned();
                });
                expanded
            }
            Ok(None) => raw_prompt.clone(),
            Err(error) => {
                return self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("Custom command was not submitted: {error}"),
                    })
                    .await;
            }
        };

        let prompt_hooks = self
            .run_hook_event(
                HookEvent::UserPromptSubmit,
                None,
                serde_json::json!({
                    "event": "user_prompt_submit",
                    "raw_prompt": &raw_prompt,
                    "expanded_prompt": &prompt,
                    "session_id": self.current_session_id.as_ref(),
                }),
                &CancellationToken::new(),
            )
            .await;
        self.publish_hook_notes(&prompt_hooks);
        if let HookDisposition::Deny { hook_id, message } = prompt_hooks.disposition {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Prompt blocked by hook {hook_id}: {message}"),
                })
                .await;
        }

        // Pick up edits to AGENTS.md immediately before budgeting and sending
        // the request. Discovery is filesystem-bound and therefore kept off
        // the actor thread; a failed refresh preserves the last good catalog.
        self.reload_project_instructions(false).await;
        self.reload_skills(false).await;

        if attachment_paths.len() > MAX_ATTACHMENTS_PER_TURN {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!(
                        "Prompt has too many attachments; at most {MAX_ATTACHMENTS_PER_TURN} are allowed"
                    ),
                })
                .await;
        }
        let attachments = match self
            .attachment_store
            .stage_many(&self.tool_runner.sandbox_root(), &attachment_paths)
        {
            Ok(attachments) => attachments,
            Err(error) => {
                return self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("Attachments were not submitted: {error}"),
                    })
                    .await;
            }
        };

        let context_budget = self.request_context_budget();
        if let Err(error) = self
            .state
            .validate_next_prompt_budget(&prompt, context_budget)
        {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!(
                        "Prompt rejected before API call: {error}. The budget already reserves instructions and max output tokens."
                    ),
                })
                .await;
        }

        if let Some(abandoned_turn) = self.retryable_turn.take() {
            self.state.clear_turn_metrics(abandoned_turn);
            self.state.mark_turn_failed(abandoned_turn);
            self.state.finish_turn(abandoned_turn);
            if self.whip_retry_note_pending == Some(abandoned_turn) {
                self.whip_retry_note_pending = None;
            }
            if !self.finalize_checkpoint().await {
                return false;
            }
        }
        let session_seed = if prompt.trim().is_empty() {
            attachments.first().map_or_else(
                || "Attachment turn".to_owned(),
                |item| item.filename.clone(),
            )
        } else {
            prompt.clone()
        };
        if !self.ensure_current_session(&session_seed).await {
            return false;
        }
        if let Some(store) = &mut self.checkpoint_store {
            match store
                .begin(
                    &prompt,
                    &self.state,
                    self.current_session_id.as_ref().map(ToString::to_string),
                )
                .await
            {
                Ok(checkpoint) => self.pending_checkpoint = Some(checkpoint),
                Err(error) => {
                    return self
                        .emit(OrchestratorEvent::RecoverableError {
                            turn_id: None,
                            message: format!(
                                "Turn was not started because its Git checkpoint could not be created: {error}"
                            ),
                        })
                        .await;
                }
            }
        }
        let turn_id = self.allocate_turn_id();
        if let Some(id) = queued_follow_up_id
            && let Err(error) = self.state.follow_ups.begin_dispatch(id, turn_id)
        {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Queued follow-up was not dispatched: {error}"),
                })
                .await;
        }
        self.state
            .push_pending_user_with_attachments(turn_id, prompt, attachments);
        self.state.begin_turn(turn_id);
        if queued_follow_up_id.is_some() {
            self.publish_follow_ups_status(UiNotice::FollowUpDispatched);
        }
        let alive = self.emit_history_durable().await && self.drive_turn(turn_id).await;
        if let Some(id) = queued_follow_up_id {
            self.reconcile_dispatched_follow_up(id, turn_id).await;
        }
        alive
    }

    #[tracing::instrument(
        name = "agent.turn",
        level = "info",
        skip_all,
        fields(
            session_id = ?self.current_session_id,
            turn_id,
            provider = ?self.provider,
            model = %self.deployment,
            status = "active"
        )
    )]
    async fn drive_turn(&mut self, turn_id: TurnId) -> bool {
        let turn_started = Instant::now();
        let usage_before = self.usage_snapshot();
        self.state.begin_turn(turn_id);
        let turn_cancel = CancellationToken::new();
        self.urgent_control
            .activate_turn(turn_id, turn_cancel.clone());
        self.active_review = None;
        let mut preflight_exit = None;
        if self.state.work_modes.review {
            if !self
                .set_phase(Some(turn_id), AgentPhase::PreparingReview)
                .await
            {
                self.urgent_control.clear_turn(turn_id);
                return false;
            }
            match DiffSnapshot::capture_with_privacy(
                &self.agent_config.workspace_root,
                self.agent_config.exec_timeout,
                &turn_cancel,
                Some(&self.privacy),
            )
            .await
            {
                Ok(snapshot) => {
                    let path_count = snapshot.changed_paths.len();
                    let diff_bytes = snapshot.diff.len();
                    self.snapshot_tx.send_modify(|ui| {
                        ui.status = format!(
                            "Review snapshot locked: {path_count} path(s), {diff_bytes} bytes"
                        );
                    });
                    self.active_review = Some(snapshot);
                }
                Err(_) if turn_cancel.is_cancelled() => {
                    preflight_exit = Some(TurnExit::Interrupted)
                }
                Err(error) => {
                    preflight_exit = Some(TurnExit::Failed(format!(
                        "Review Mode could not capture an immutable Git diff; no API request was sent: {error}"
                    )));
                }
            }
        }
        let already_planned = self.state.history.iter().any(|entry| {
            entry.turn_id == turn_id
                && matches!(entry.kind, HistoryKind::Assistant)
                && entry.content.starts_with("[Approved implementation plan]")
        });
        let turn_exit = if let Some(exit) = preflight_exit {
            exit
        } else if self.state.work_modes.plan && !already_planned {
            match self.run_plan_phase(turn_id, &turn_cancel).await {
                Ok(()) => self.run_logical_turn(turn_id, turn_cancel.clone()).await,
                Err(exit) => exit,
            }
        } else {
            self.run_logical_turn(turn_id, turn_cancel.clone()).await
        };
        self.active_review = None;
        if matches!(turn_exit, TurnExit::Completed) {
            let completion_hooks = self
                .run_hook_event(
                    HookEvent::TurnComplete,
                    None,
                    serde_json::json!({
                        "event": "turn_complete",
                        "turn_id": turn_id,
                        "session_id": self.current_session_id.as_ref(),
                    }),
                    &turn_cancel,
                )
                .await;
            self.publish_hook_notes(&completion_hooks);
        }
        self.urgent_control.clear_turn(turn_id);
        // Double-hit classification is scoped to one logical turn.
        self.last_whip = None;
        let usage_after = self.usage_snapshot();
        let attempt_metrics = turn_metrics_since(
            &usage_before,
            &usage_after,
            &self.deployment,
            turn_started.elapsed(),
        );
        match turn_exit {
            TurnExit::Completed => {
                let _ = self.urgent_control.take_pause_request(turn_id);
                self.retryable_turn = None;
                if self.pause_resume_note_pending == Some(turn_id) {
                    self.pause_resume_note_pending = None;
                }
                let _ = self.state.complete_turn_metrics(turn_id, attempt_metrics);
                self.state.mark_turn_committed(turn_id);
                self.state.finish_turn(turn_id);
                self.finalize_checkpoint().await
                    && self.emit_history_durable().await
                    && self.set_phase(Some(turn_id), AgentPhase::Idle).await
                    && self.emit(OrchestratorEvent::Done { turn_id }).await
                    && self.publish_sessions(String::new(), false).await
            }
            TurnExit::Interrupted => {
                let explicitly_paused = self.urgent_control.take_pause_request(turn_id);
                if explicitly_paused {
                    self.retryable_turn = Some(turn_id);
                    self.state.accumulate_turn_metrics(turn_id, attempt_metrics);
                    self.state.mark_turn_paused(turn_id);
                    self.state.finish_turn(turn_id);
                    if self.whip_retry_note_pending == Some(turn_id) {
                        self.whip_retry_note_pending = None;
                    }
                    return self.finalize_checkpoint().await
                        && self.emit_history_durable().await
                        && self.set_phase(None, AgentPhase::Idle).await
                        && self.emit(OrchestratorEvent::TurnPaused { turn_id }).await
                        && self.publish_sessions(String::new(), false).await;
                }
                self.retryable_turn = None;
                self.state.clear_turn_metrics(turn_id);
                if self.pause_resume_note_pending == Some(turn_id) {
                    self.pause_resume_note_pending = None;
                }
                if self.whip_retry_note_pending == Some(turn_id) {
                    self.whip_retry_note_pending = None;
                }
                self.state
                    .follow_ups
                    .fail_pending_steers_for_turn(turn_id, UiNotice::FollowUpInterrupted);
                self.publish_follow_ups_status(UiNotice::FollowUpInterrupted);
                self.state.mark_turn_cancelled(turn_id);
                self.state.finish_turn(turn_id);
                self.finalize_checkpoint().await
                    && self.emit_history_durable().await
                    && self.set_phase(Some(turn_id), AgentPhase::Idle).await
                    && self.emit(OrchestratorEvent::Done { turn_id }).await
                    && self.publish_sessions(String::new(), false).await
            }
            TurnExit::Reset => {
                self.retryable_turn = None;
                self.pause_resume_note_pending = None;
                self.state.clear_turn_metrics(turn_id);
                if !self.finalize_checkpoint().await {
                    return false;
                }
                self.reset_state().await;
                true
            }
            TurnExit::Shutdown => {
                self.state.accumulate_turn_metrics(turn_id, attempt_metrics);
                self.prepare_shutdown(turn_id).await;
                false
            }
            TurnExit::Failed(message) => {
                let _ = self.urgent_control.take_pause_request(turn_id);
                self.retryable_turn = Some(turn_id);
                self.state.accumulate_turn_metrics(turn_id, attempt_metrics);
                let phase = AgentPhase::Error {
                    message: message.clone(),
                    recoverable: true,
                };
                self.emit_history_durable().await
                    && self.set_phase(Some(turn_id), phase).await
                    && self
                        .emit(OrchestratorEvent::RecoverableError {
                            turn_id: Some(turn_id),
                            message,
                        })
                        .await
            }
        }
    }

    async fn run_plan_phase(
        &mut self,
        turn_id: TurnId,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        if !self.set_phase(Some(turn_id), AgentPhase::Planning).await {
            return Err(TurnExit::Shutdown);
        }
        let mut plan_instructions = self.effective_instructions();
        plan_instructions.push_str("\n\n");
        plan_instructions.push_str(PLAN_INSTRUCTIONS);
        let maximum_replay_budget = self.request_context_budget_for(&plan_instructions);
        let normal = self.build_plan_request_with_context_budget(
            plan_instructions.clone(),
            maximum_replay_budget,
        );
        let exact = self.build_plan_request_for_exact_preflight(
            plan_instructions.clone(),
            maximum_replay_budget,
        );
        let use_exact = exact
            .as_ref()
            .is_ok_and(|(request, _, _)| self.should_preflight_exact_tokens(request));
        let (request, reasoning_effort, reasoning_mode) = if use_exact {
            let (request, reasoning_effort, reasoning_mode) =
                exact.map_err(|error| TurnExit::Failed(error.to_string()))?;
            let fallback = normal.ok().map(|(request, _, _)| request);
            let fitted = self
                .fit_exact_request(
                    request,
                    fallback,
                    maximum_replay_budget,
                    turn_cancel,
                    move |orchestrator, budget| {
                        orchestrator
                            .build_plan_request_for_exact_preflight(
                                plan_instructions.clone(),
                                budget,
                            )
                            .map(|(request, _, _)| request)
                    },
                )
                .await
                .map_err(TurnExit::Failed)?;
            (fitted, reasoning_effort, reasoning_mode)
        } else {
            normal.map_err(|error| TurnExit::Failed(error.to_string()))?
        };

        let completed = {
            let mut network_attempt = 1_u32;
            loop {
                match self
                    .run_api_attempt(turn_id, request.clone(), turn_cancel.child_token(), false)
                    .await
                {
                    AttemptExit::Failed { partial, error }
                        if partial.is_empty()
                            && self.client.is_retryable(&error)
                            && network_attempt < self.client.max_attempts() =>
                    {
                        let _ = self
                            .emit(OrchestratorEvent::RetryScheduled {
                                conversation_epoch: self.conversation_epoch,
                                turn_id,
                                next_attempt: network_attempt.saturating_add(1),
                                max_attempts: self.client.max_attempts(),
                                reason: error.to_string(),
                            })
                            .await;
                        match self
                            .wait_before_network_retry(
                                turn_id,
                                &error,
                                network_attempt,
                                turn_cancel,
                            )
                            .await
                        {
                            RetryWaitExit::Ready => {
                                network_attempt = network_attempt.saturating_add(1);
                            }
                            RetryWaitExit::Interrupted => {
                                return Err(TurnExit::Interrupted);
                            }
                            RetryWaitExit::Reset => return Err(TurnExit::Reset),
                            RetryWaitExit::Shutdown => return Err(TurnExit::Shutdown),
                        }
                    }
                    completed => break completed,
                }
            }
        };
        let (response, text) = match completed {
            AttemptExit::Completed { response, text, .. } => (response, text),
            AttemptExit::WhipRetry { partial } => {
                let _ = self.record_superseded(turn_id, partial).await;
                return Err(TurnExit::Interrupted);
            }
            AttemptExit::Interrupted { partial } => {
                let _ = self.record_interrupted(turn_id, partial).await;
                return Err(TurnExit::Interrupted);
            }
            AttemptExit::Reset { partial } => {
                let _ = self.record_interrupted(turn_id, partial).await;
                return Err(TurnExit::Reset);
            }
            AttemptExit::Shutdown => return Err(TurnExit::Shutdown),
            AttemptExit::Failed { partial, error } => {
                let _ = self.record_failed_draft(turn_id, partial).await;
                return Err(TurnExit::Failed(format!(
                    "read-only planning pass failed without executing tools: {error}"
                )));
            }
        };
        if !self
            .record_uncommitted_response_usage(turn_id, &response)
            .await
        {
            return Err(TurnExit::Shutdown);
        }
        let unexpected_calls = response
            .function_calls()
            .map_err(|error| TurnExit::Failed(error.to_string()))?;
        if !unexpected_calls.is_empty() {
            return Err(TurnExit::Failed(
                "planning response attempted a native tool call; the read-only harness rejected the whole plan"
                    .to_owned(),
            ));
        }
        let plan = validate_plan(text).map_err(|error| TurnExit::Failed(error.to_string()))?;
        let review_id = self.allocate_plan_review_id();
        let review = Arc::new(PlanReview {
            turn_id,
            review_id,
            plan,
            deployment: self.deployment.clone(),
            reasoning_effort,
            reasoning_mode,
        });
        match self
            .await_plan_approval(Arc::clone(&review), turn_cancel)
            .await?
        {
            PlanDecision::Approve { plan } => {
                let plan =
                    validate_plan(plan).map_err(|error| TurnExit::Failed(error.to_string()))?;
                self.state
                    .push_assistant(turn_id, format!("[Approved implementation plan]\n{plan}"));
                self.state.push_user(
                    turn_id,
                    "The user approved the implementation plan above. Execute it now; keep any material deviation explicit and preserve the original request's authority boundaries.",
                );
                self.state.mark_turn_committed(turn_id);
                if !self.emit_history_durable().await {
                    return Err(TurnExit::Shutdown);
                }
                Ok(())
            }
            PlanDecision::Reject => {
                self.state.push_assistant(
                    turn_id,
                    format!(
                        "[Rejected read-only plan; no tools were executed]\n{}",
                        review.plan
                    ),
                );
                self.state.mark_turn_committed(turn_id);
                Err(TurnExit::Interrupted)
            }
        }
    }

    async fn await_plan_approval(
        &mut self,
        review: Arc<PlanReview>,
        turn_cancel: &CancellationToken,
    ) -> Result<PlanDecision, TurnExit> {
        if self.state.auto_approval.plans {
            self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status =
                    "Plan auto-approved by the session Auto-Approval Center".to_owned();
            });
            return Ok(PlanDecision::Approve {
                plan: review.plan.clone(),
            });
        }
        if !self
            .set_phase(Some(review.turn_id), AgentPhase::AwaitingPlanApproval)
            .await
        {
            return Err(TurnExit::Shutdown);
        }
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.modal = Some(UiModal::PlanApproval {
                review: Arc::clone(&review),
            });
            snapshot.status = "Review the read-only plan: approve, edit, or reject".to_owned();
        });
        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => match self.drain_urgent_busy_controls(review.turn_id) {
                    AwaitedControl::Continue => {}
                    AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                    AwaitedControl::Reset => return Err(TurnExit::Reset),
                    AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                },
                _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        return Err(TurnExit::Shutdown);
                    };
                    match command {
                        OrchestratorCommand::DecidePlan { turn_id, review_id, decision }
                            if turn_id == review.turn_id && review_id == review.review_id =>
                        {
                            return Ok(decision);
                        }
                        other => match self.handle_busy_control(other, review.turn_id).await {
                            AwaitedControl::Continue => {}
                            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                            AwaitedControl::Reset => return Err(TurnExit::Reset),
                            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                        },
                    }
                }
            }
        }
    }

    #[tracing::instrument(
        name = "agent.logical_turn",
        level = "debug",
        skip_all,
        fields(
            session_id = ?self.current_session_id,
            turn_id,
            provider = ?self.provider,
            model = %self.deployment,
            status = "active"
        )
    )]
    async fn run_logical_turn(
        &mut self,
        turn_id: TurnId,
        turn_cancel: CancellationToken,
    ) -> TurnExit {
        let chunk = self.agent_config.max_tool_iterations.max(1);
        let mut iteration_limit = chunk;
        let mut completed_iterations = 0_u32;
        let mut goal_reminder_sent = false;
        let mut review_reminder_sent = false;

        'logical_turn: loop {
            let attempt = {
                let mut network_attempt = 1_u32;
                loop {
                    let penalty_applied = self.penalty_responses_remaining > 0;
                    let maximum_replay_budget = self.main_request_context_budget(turn_id);
                    let normal_request = self.build_request(turn_id, penalty_applied);
                    let exact_request =
                        matches!(self.agent_config.context_mode, ContextMode::Stateless).then(
                            || {
                                self.build_request_for_exact_preflight(
                                    turn_id,
                                    penalty_applied,
                                    maximum_replay_budget,
                                )
                            },
                        );
                    let use_exact = exact_request
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .is_some_and(|request| self.should_preflight_exact_tokens(request));
                    let request = if use_exact {
                        let exact_request = match exact_request {
                            Some(Ok(request)) => request,
                            Some(Err(error)) => return TurnExit::Failed(error.to_string()),
                            None => unreachable!(),
                        };
                        match self
                            .fit_exact_request(
                                exact_request,
                                normal_request.ok(),
                                maximum_replay_budget,
                                &turn_cancel,
                                |orchestrator, budget| {
                                    orchestrator.build_request_for_exact_preflight(
                                        turn_id,
                                        penalty_applied,
                                        budget,
                                    )
                                },
                            )
                            .await
                        {
                            Ok(request) => request,
                            Err(error) => return TurnExit::Failed(error),
                        }
                    } else {
                        match normal_request {
                            Ok(request) => request,
                            Err(error) => {
                                return TurnExit::Failed(format!(
                                    "request was not sent because strict context compaction would discard the newest causal group: {error}"
                                ));
                            }
                        }
                    };
                    match self
                        .run_api_attempt(
                            turn_id,
                            request,
                            turn_cancel.child_token(),
                            penalty_applied,
                        )
                        .await
                    {
                        AttemptExit::WhipRetry { partial } => {
                            if !self.record_superseded(turn_id, partial).await {
                                return TurnExit::Shutdown;
                            }
                            network_attempt = 1;
                        }
                        AttemptExit::Interrupted { partial } => {
                            let _ = self.record_interrupted(turn_id, partial).await;
                            return TurnExit::Interrupted;
                        }
                        AttemptExit::Reset { partial } => {
                            let _ = self.record_interrupted(turn_id, partial).await;
                            return TurnExit::Reset;
                        }
                        AttemptExit::Shutdown => return TurnExit::Shutdown,
                        AttemptExit::Failed { partial, error }
                            if partial.is_empty()
                                && self.client.is_retryable(&error)
                                && network_attempt < self.client.max_attempts() =>
                        {
                            if !self.record_superseded(turn_id, partial).await {
                                return TurnExit::Shutdown;
                            }
                            let _ = self
                                .emit(OrchestratorEvent::RetryScheduled {
                                    conversation_epoch: self.conversation_epoch,
                                    turn_id,
                                    next_attempt: network_attempt.saturating_add(1),
                                    max_attempts: self.client.max_attempts(),
                                    reason: error.to_string(),
                                })
                                .await;
                            match self
                                .wait_before_network_retry(
                                    turn_id,
                                    &error,
                                    network_attempt,
                                    &turn_cancel,
                                )
                                .await
                            {
                                RetryWaitExit::Ready => {
                                    network_attempt = network_attempt.saturating_add(1);
                                }
                                RetryWaitExit::Interrupted => {
                                    return TurnExit::Interrupted;
                                }
                                RetryWaitExit::Reset => return TurnExit::Reset,
                                RetryWaitExit::Shutdown => return TurnExit::Shutdown,
                            }
                        }
                        AttemptExit::Failed { partial, error }
                            if !partial.is_empty() && self.client.is_retryable(&error) =>
                        {
                            let _ = self.record_failed_draft(turn_id, partial).await;
                            return TurnExit::Failed(format!(
                                "the stream disconnected after output had started; automatic reconnect was stopped to avoid duplicate work and an endless Azure retry loop. Review the preserved draft, then click Retry or Abort. Last transport error: {error}"
                            ));
                        }
                        AttemptExit::Failed { partial, error }
                            if self.client.is_retryable(&error) =>
                        {
                            let _ = self.record_failed_draft(turn_id, partial).await;
                            return TurnExit::Failed(
                                ApiError::RetryExhausted {
                                    attempts: network_attempt,
                                    last_error: error.to_string(),
                                }
                                .to_string(),
                            );
                        }
                        AttemptExit::Failed { partial, error } => {
                            let _ = self.record_failed_draft(turn_id, partial).await;
                            return TurnExit::Failed(error.to_string());
                        }
                        completed @ AttemptExit::Completed { .. } => break completed,
                    }
                }
            };

            let AttemptExit::Completed {
                response,
                text,
                penalty_applied,
            } = attempt
            else {
                return TurnExit::Failed("invalid orchestrator state".to_owned());
            };
            if self.pause_resume_note_pending == Some(turn_id) {
                self.pause_resume_note_pending = None;
            }

            // Terminal latch won the stream race. Whip is now stale, while
            // Interrupt/Reset/Shutdown still preempt before history mutation.
            match self.drain_busy_controls(turn_id).await {
                AwaitedControl::Continue => {}
                AwaitedControl::Interrupt => {
                    let _ = self.record_interrupted(turn_id, text).await;
                    return TurnExit::Interrupted;
                }
                AwaitedControl::Reset => return TurnExit::Reset,
                AwaitedControl::Shutdown => return TurnExit::Shutdown,
            }

            // The transient retry note is repeated across transport retries but
            // disappears after the first authoritative completed response.
            if self.whip_retry_note_pending == Some(turn_id) {
                self.whip_retry_note_pending = None;
            }

            if penalty_applied {
                self.penalty_responses_remaining =
                    self.penalty_responses_remaining.saturating_sub(1);
                let estimated_reduction =
                    self.base_max_output_tokens
                        .saturating_sub(penalized_output_tokens(
                            self.base_max_output_tokens,
                            &self.agent_config.whip,
                        ));
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.whip.penalty_responses_remaining = self.penalty_responses_remaining;
                    snapshot.whip.estimated_saved_token_budget = snapshot
                        .whip
                        .estimated_saved_token_budget
                        .saturating_add(u64::from(estimated_reduction));
                });
            }

            let native_calls = match response.function_calls() {
                Ok(calls) => calls,
                Err(error) => {
                    if !self
                        .record_uncommitted_response_usage(turn_id, &response)
                        .await
                    {
                        return TurnExit::Shutdown;
                    }
                    let _ = self.record_failed_draft(turn_id, text).await;
                    return TurnExit::Failed(format!(
                        "completed response contained a malformed native function call: {error}"
                    ));
                }
            };

            let assistant_sequence =
                self.state
                    .push_assistant_with_api_items(turn_id, &text, response.replay_items());
            // A validated completed response makes the initial prompt durable,
            // even if a later tool round needs an explicit RetryTurn.
            self.state.mark_turn_committed(turn_id);
            if let Some(usage) = &response.usage {
                self.state.record_deployment_usage(
                    &self.deployment,
                    usage.input_tokens,
                    usage.cached_input_tokens(),
                    usage.output_tokens,
                    usage.total_tokens,
                    assistant_sequence,
                );
                if !self
                    .emit(OrchestratorEvent::Usage {
                        turn_id,
                        usage: self.usage_snapshot(),
                    })
                    .await
                {
                    return TurnExit::Shutdown;
                }
            }
            if matches!(self.agent_config.context_mode, ContextMode::Stateful) {
                self.state.last_response_id = Some(response.id);
                self.state.mark_represented_through(assistant_sequence);
            }

            if !self
                .emit(OrchestratorEvent::AssistantCommitted {
                    turn_id,
                    content: visible_assistant_text(&text),
                })
                .await
                || !self.emit_history().await
            {
                return TurnExit::Shutdown;
            }

            if !self.set_phase(Some(turn_id), AgentPhase::Parsing).await {
                return TurnExit::Shutdown;
            }
            let parsed = match self.parse_with_controls(turn_id, text, &turn_cancel).await {
                Ok(parsed) => parsed,
                Err(exit) => return exit,
            };
            let has_follow_up = !native_calls.is_empty()
                || parsed.iter().any(|event| {
                    matches!(
                        event,
                        ParserEvent::ToolCallParsed(_) | ParserEvent::ToolCallParseError { .. }
                    )
                });

            if !has_follow_up {
                let mut no_native_calls = VecDeque::new();
                let mut no_parser_events = VecDeque::new();
                match self
                    .deliver_pending_steer(turn_id, &mut no_native_calls, &mut no_parser_events)
                    .await
                {
                    Ok(true) => continue 'logical_turn,
                    Ok(false) => {}
                    Err(exit) => return exit,
                }
                let goal_needs_update = self
                    .state
                    .work_modes
                    .goal
                    .as_ref()
                    .is_some_and(|goal| goal.last_checked_turn != Some(turn_id));
                if goal_needs_update && !goal_reminder_sent {
                    goal_reminder_sent = true;
                    let action_id = self.allocate_action_id();
                    self.state.push_tool_diagnostic(
                        turn_id,
                        action_id,
                        "goal_guard",
                        ToolResultStatus::Failure,
                        "Goal Mode requires one update_goal self-check before completing this turn. Reconcile progress with the persistent top-level objective, verify the evidence, then call update_goal. This guard is emitted once and will not loop indefinitely.",
                    );
                    if !self.emit_history().await {
                        return TurnExit::Shutdown;
                    }
                    continue;
                }
                if goal_needs_update {
                    return TurnExit::Failed(
                        "Goal Mode did not update persistent progress after one explicit reminder. Retry or abort the turn."
                            .to_owned(),
                    );
                }
                let review_needs_submission =
                    self.state.work_modes.review && !self.state.reviews.submitted_for_turn(turn_id);
                if review_needs_submission && !review_reminder_sent {
                    review_reminder_sent = true;
                    let action_id = self.allocate_action_id();
                    self.state.push_tool_diagnostic(
                        turn_id,
                        action_id,
                        "review_guard",
                        ToolResultStatus::Failure,
                        "Review Mode requires exactly one submit_review call bound to the active immutable diff before this turn can complete. Page review_diff through complete=true, then submit concrete findings or an explicit pass. This guard is emitted once and cannot loop indefinitely.",
                    );
                    if !self.emit_history().await {
                        return TurnExit::Shutdown;
                    }
                    continue;
                }
                if review_needs_submission {
                    return TurnExit::Failed(
                        "Review Mode did not submit a structured snapshot-bound report after one explicit reminder; no writable action was executed. Retry or abort the turn."
                            .to_owned(),
                    );
                }
                return TurnExit::Completed;
            }

            if self.state.follow_ups.next_steer(turn_id).is_some() {
                let mut pending_native = VecDeque::from(native_calls.clone());
                let mut pending_parser = VecDeque::from(parsed.clone());
                match self
                    .deliver_pending_steer(turn_id, &mut pending_native, &mut pending_parser)
                    .await
                {
                    Ok(true) => continue 'logical_turn,
                    Ok(false) => {}
                    Err(exit) => return exit,
                }
            }

            if completed_iterations >= iteration_limit {
                match self
                    .await_continuation(
                        turn_id,
                        completed_iterations,
                        iteration_limit,
                        &turn_cancel,
                    )
                    .await
                {
                    Ok(true) => iteration_limit = iteration_limit.saturating_add(chunk),
                    Ok(false) => {
                        if !self.record_stopped_batch(turn_id, &parsed).await {
                            return TurnExit::Shutdown;
                        }
                        return TurnExit::Completed;
                    }
                    Err(exit) => return exit,
                }
            }

            completed_iterations = completed_iterations.saturating_add(1);
            let mut native_calls = VecDeque::from(native_calls);
            let mut parsed = VecDeque::from(parsed);
            match self
                .deliver_pending_steer(turn_id, &mut native_calls, &mut parsed)
                .await
            {
                Ok(true) => continue 'logical_turn,
                Ok(false) => {}
                Err(exit) => return exit,
            }
            while let Some(call) = native_calls.pop_front() {
                if let Err(exit) = self.run_mcp_call(turn_id, call, &turn_cancel).await {
                    return exit;
                }
                match self
                    .deliver_pending_steer(turn_id, &mut native_calls, &mut parsed)
                    .await
                {
                    Ok(true) => continue 'logical_turn,
                    Ok(false) => {}
                    Err(exit) => return exit,
                }
            }
            while let Some(event) = parsed.pop_front() {
                match event {
                    ParserEvent::ToolCallParsed(action) => {
                        let batch = collect_parallel_read_batch(action, &mut parsed);
                        let result = if batch.len() > 1 && self.read_batch_has_no_hooks(&batch) {
                            self.run_read_batch(turn_id, batch, &turn_cancel).await
                        } else {
                            let mut actions = batch.into_iter();
                            let Some(action) = actions.next() else {
                                return TurnExit::Failed(
                                    "internal read batch unexpectedly became empty".to_owned(),
                                );
                            };
                            for pending_action in actions.rev() {
                                parsed.push_front(ParserEvent::ToolCallParsed(pending_action));
                            }
                            self.run_action(turn_id, action, &turn_cancel).await
                        };
                        if let Err(exit) = result {
                            return exit;
                        }
                    }
                    ParserEvent::ToolCallParseError { raw_tag, reason } => {
                        let action_id = self.allocate_action_id();
                        let message =
                            format!("Tool protocol parse error: {reason}\nRaw tag: {raw_tag}");
                        self.state.push_tool_diagnostic(
                            turn_id,
                            action_id,
                            "parser_error",
                            ToolResultStatus::ParseError,
                            &message,
                        );
                        if !self
                            .emit(OrchestratorEvent::RecoverableError {
                                turn_id: Some(turn_id),
                                message,
                            })
                            .await
                            || !self.emit_history().await
                        {
                            return TurnExit::Shutdown;
                        }
                    }
                    ParserEvent::ThinkingDelta(_)
                    | ParserEvent::ThinkingEnd
                    | ParserEvent::TurnComplete { .. } => {}
                }
                match self
                    .deliver_pending_steer(turn_id, &mut native_calls, &mut parsed)
                    .await
                {
                    Ok(true) => continue 'logical_turn,
                    Ok(false) => {}
                    Err(exit) => return exit,
                }
            }
        }
    }

    async fn deliver_pending_steer(
        &mut self,
        turn_id: TurnId,
        native_calls: &mut VecDeque<FunctionCall>,
        parser_events: &mut VecDeque<ParserEvent>,
    ) -> Result<bool, TurnExit> {
        let Some(pending) = self.state.follow_ups.next_steer(turn_id).cloned() else {
            return Ok(false);
        };
        while let Some(call) = native_calls.pop_front() {
            let action_id = self.allocate_action_id();
            let output = serde_json::json!({
                "ok": false,
                "error": "Skipped before execution because the user delivered an explicit Steer at a safe tool boundary",
            })
            .to_string();
            let sequence = self.state.push_tool_diagnostic(
                turn_id,
                action_id,
                format!("steer_skip:{}", call.name),
                ToolResultStatus::Declined,
                &output,
            );
            let _attached = self.state.set_api_items(
                sequence,
                vec![serde_json::json!({
                    "type": "function_call_output",
                    "call_id": call.call_id,
                    "output": output,
                })],
            );
        }
        while let Some(event) = parser_events.pop_front() {
            let (tool_name, message) = match event {
                ParserEvent::ToolCallParsed(action) => (
                    format!("steer_skip:{}", action.tool_name()),
                    "Legacy tool action skipped before execution because the user delivered an explicit Steer at a safe boundary".to_owned(),
                ),
                ParserEvent::ToolCallParseError { reason, .. } => (
                    "steer_skip:parser_error".to_owned(),
                    format!("Malformed legacy action superseded by Steer: {reason}"),
                ),
                ParserEvent::ThinkingDelta(_)
                | ParserEvent::ThinkingEnd
                | ParserEvent::TurnComplete { .. } => continue,
            };
            let action_id = self.allocate_action_id();
            self.state.push_tool_diagnostic(
                turn_id,
                action_id,
                tool_name,
                ToolResultStatus::Declined,
                message,
            );
        }
        let delivered = self
            .state
            .follow_ups
            .deliver_steer(pending.id, turn_id)
            .map_err(|error| TurnExit::Failed(format!("Steer delivery failed closed: {error}")))?;
        self.state.push_user(
            turn_id,
            format!(
                "[User Steer #{} delivered at a safe tool boundary; supersede any skipped proposed actions and adjust the active task without discarding verified completed work]\n{}",
                delivered.id, delivered.text
            ),
        );
        self.publish_follow_ups_status(UiNotice::FollowUpDeliveredInsideTurn { turn_id });
        if !self.emit_history_durable().await {
            return Err(TurnExit::Shutdown);
        }
        Ok(true)
    }

    async fn run_api_attempt(
        &mut self,
        turn_id: TurnId,
        mut request: ResponsesRequest,
        attempt_cancel: CancellationToken,
        penalty_applied: bool,
    ) -> AttemptExit {
        if let Err(error) = self
            .attachment_store
            .hydrate_request(&mut request, self.client.capabilities())
        {
            return AttemptExit::Failed {
                partial: String::new(),
                error: ApiError::Protocol(format!(
                    "attachment request was rejected before network I/O: {error}"
                )),
            };
        }
        if !self.set_phase(Some(turn_id), AgentPhase::Requesting).await {
            return AttemptExit::Shutdown;
        }

        let client = Arc::clone(&self.client);
        let request_cancel = attempt_cancel.clone();
        let request_future = async move {
            client
                .stream_response_attempt(request, request_cancel)
                .await
        };
        tokio::pin!(request_future);
        let mut soft_boundary_from = None;
        let urgent = self.urgent_control.clone();

        let mut stream = loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self
                        .drain_urgent_stream_controls(
                            turn_id,
                            0,
                            &mut soft_boundary_from,
                        )
                        .await
                    {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            attempt_cancel.cancel();
                            return AttemptExit::Interrupted { partial: String::new() };
                        }
                        AwaitedControl::Reset => {
                            attempt_cancel.cancel();
                            return AttemptExit::Reset { partial: String::new() };
                        }
                        AwaitedControl::Shutdown => {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from.is_some() {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry { partial: String::new() };
                    }
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        attempt_cancel.cancel();
                        return AttemptExit::Shutdown;
                    };
                    match self
                        .handle_stream_control(
                            command,
                            turn_id,
                            0,
                            &mut soft_boundary_from,
                        )
                        .await
                    {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            attempt_cancel.cancel();
                            return AttemptExit::Interrupted {
                                partial: String::new(),
                            };
                        }
                        AwaitedControl::Reset => {
                            attempt_cancel.cancel();
                            return AttemptExit::Reset {
                                partial: String::new(),
                            };
                        }
                        AwaitedControl::Shutdown => {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from.is_some() {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry {
                            partial: String::new(),
                        };
                    }
                }
                result = &mut request_future => {
                    match result {
                        Ok(stream) => break stream,
                        Err(error) => {
                            return AttemptExit::Failed {
                                partial: String::new(),
                                error,
                            };
                        }
                    }
                }
            }
        };

        if !self.set_phase(Some(turn_id), AgentPhase::Streaming).await {
            attempt_cancel.cancel();
            return AttemptExit::Shutdown;
        }

        let mut full_response = String::new();
        let mut final_text = None;
        let mut live_preview = LivePreview::new();
        let mut boundary_tracker = ProtocolBoundaryTracker::default();
        let urgent = self.urgent_control.clone();

        'stream_loop: loop {
            // Keep one `StreamExt::next` future alive while ordinary UI
            // commands are handled. Recreating it after another select branch
            // wins can cancel an in-flight HTTP body read; some hyper/reqwest
            // decoders then surface a spurious body error and lose an otherwise
            // complete response.
            let next_event = stream.next();
            tokio::pin!(next_event);
            let next = loop {
                tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self
                        .drain_urgent_stream_controls(
                            turn_id,
                            full_response.len(),
                            &mut soft_boundary_from,
                        )
                        .await
                    {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            attempt_cancel.cancel();
                            return AttemptExit::Interrupted { partial: full_response };
                        }
                        AwaitedControl::Reset => {
                            attempt_cancel.cancel();
                            return AttemptExit::Reset { partial: full_response };
                        }
                        AwaitedControl::Shutdown => {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from == Some(usize::MAX)
                        || (soft_boundary_from.is_some() && boundary_tracker.is_at_boundary())
                    {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry { partial: full_response };
                    }
                    continue;
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                    continue;
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        attempt_cancel.cancel();
                        return AttemptExit::Shutdown;
                    };
                    match self
                        .handle_stream_control(
                            command,
                            turn_id,
                            full_response.len(),
                            &mut soft_boundary_from,
                        )
                        .await
                    {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            attempt_cancel.cancel();
                            return AttemptExit::Interrupted {
                                partial: full_response,
                            };
                        }
                        AwaitedControl::Reset => {
                            attempt_cancel.cancel();
                            return AttemptExit::Reset {
                                partial: full_response,
                            };
                        }
                        AwaitedControl::Shutdown => {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from == Some(usize::MAX)
                        || (soft_boundary_from.is_some() && boundary_tracker.is_at_boundary())
                    {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry {
                            partial: full_response,
                        };
                    }
                    continue;
                }
                next = &mut next_event => break next,
                }
            };
            match next {
                Some(Ok(StreamEvent::OutputTextDelta { delta })) => {
                    let closed_outer = boundary_tracker.feed(&delta);
                    full_response.push_str(&delta);
                    for preview_event in live_preview.feed(&delta) {
                        if let ParserEvent::ThinkingDelta(delta) = preview_event
                            && !self
                                .emit_transient(OrchestratorEvent::ThinkingDelta { turn_id, delta })
                        {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from.is_some()
                        && soft_boundary_from != Some(usize::MAX)
                        && (closed_outer || boundary_tracker.is_at_boundary())
                    {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry {
                            partial: full_response,
                        };
                    }
                }
                Some(Ok(StreamEvent::OutputTextDone { text })) => {
                    final_text = Some(text);
                }
                Some(Ok(StreamEvent::Completed { response })) => {
                    // This select branch is the terminal latch. If a
                    // Whip had already been ready, the biased urgent or
                    // command branch above would have won. Whips that
                    // arrive after this point are stale; only
                    // Interrupt/Reset/Shutdown may still preempt the
                    // upcoming history mutation.
                    match self.drain_busy_controls(turn_id).await {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            attempt_cancel.cancel();
                            return AttemptExit::Interrupted {
                                partial: full_response,
                            };
                        }
                        AwaitedControl::Reset => {
                            attempt_cancel.cancel();
                            return AttemptExit::Reset {
                                partial: full_response,
                            };
                        }
                        AwaitedControl::Shutdown => {
                            attempt_cancel.cancel();
                            return AttemptExit::Shutdown;
                        }
                    }
                    if soft_boundary_from.is_some() {
                        attempt_cancel.cancel();
                        return AttemptExit::WhipRetry {
                            partial: full_response,
                        };
                    }
                    if let Err(error) = validate_completed_status(&response) {
                        if !self
                            .record_uncommitted_response_usage(turn_id, &response)
                            .await
                        {
                            return AttemptExit::Shutdown;
                        }
                        return AttemptExit::Failed {
                            partial: full_response,
                            error,
                        };
                    }
                    for preview_event in live_preview.finish() {
                        if let ParserEvent::ThinkingDelta(delta) = preview_event {
                            let _ = self.emit_transient(OrchestratorEvent::ThinkingDelta {
                                turn_id,
                                delta,
                            });
                        }
                    }
                    let terminal_text = terminal_text(final_text, full_response, &response);
                    break 'stream_loop AttemptExit::Completed {
                        response,
                        text: terminal_text,
                        penalty_applied,
                    };
                }
                Some(Ok(StreamEvent::Failed { response })) => {
                    if !self
                        .record_uncommitted_response_usage(turn_id, &response)
                        .await
                    {
                        return AttemptExit::Shutdown;
                    }
                    return AttemptExit::Failed {
                        partial: full_response,
                        error: response_error("failed", &response),
                    };
                }
                Some(Ok(StreamEvent::Incomplete { response })) => {
                    if !self
                        .record_uncommitted_response_usage(turn_id, &response)
                        .await
                    {
                        return AttemptExit::Shutdown;
                    }
                    return AttemptExit::Failed {
                        partial: full_response,
                        error: response_error("incomplete", &response),
                    };
                }
                Some(Ok(StreamEvent::Cancelled { response })) => {
                    if let Some(response) = response.as_ref()
                        && !self
                            .record_uncommitted_response_usage(turn_id, response)
                            .await
                    {
                        return AttemptExit::Shutdown;
                    }
                    let message = response.as_ref().map_or_else(
                        || "remote response was cancelled".to_owned(),
                        |value| response_error("cancelled", value).to_string(),
                    );
                    return AttemptExit::Failed {
                        partial: full_response,
                        error: ApiError::Protocol(message),
                    };
                }
                Some(Ok(StreamEvent::Error { code, message, .. })) => {
                    return AttemptExit::Failed {
                        partial: full_response,
                        error: ApiError::remote(code.as_deref(), message),
                    };
                }
                Some(Ok(
                    StreamEvent::Created { .. } | StreamEvent::Ignored | StreamEvent::Done,
                )) => {}
                Some(Err(ApiError::Cancelled)) => {
                    return AttemptExit::Interrupted {
                        partial: full_response,
                    };
                }
                Some(Err(error)) => {
                    return AttemptExit::Failed {
                        partial: full_response,
                        error,
                    };
                }
                None => {
                    return AttemptExit::Failed {
                        partial: full_response,
                        error: ApiError::Protocol(
                            "response stream ended before response.completed".to_owned(),
                        ),
                    };
                }
            }
        }
    }

    async fn handle_stream_control(
        &mut self,
        command: OrchestratorCommand,
        turn_id: TurnId,
        response_len: usize,
        soft_boundary_from: &mut Option<usize>,
    ) -> AwaitedControl {
        let Some(command) = self.handle_side_chat_command(command).await else {
            return AwaitedControl::Continue;
        };
        let Some(command) = self.handle_follow_up_command(command).await else {
            return AwaitedControl::Continue;
        };
        let Some(command) = self.handle_subagent_command(command).await else {
            return AwaitedControl::Continue;
        };
        match command {
            OrchestratorCommand::Submit { .. } => {
                let _ = self
                    .emit(OrchestratorEvent::BusyRejected {
                        turn_id,
                        message: "A turn is already in progress.".to_owned(),
                    })
                    .await;
                AwaitedControl::Continue
            }
            OrchestratorCommand::Interrupt { turn_id: requested } if requested == turn_id => {
                AwaitedControl::Interrupt
            }
            OrchestratorCommand::Whip { turn_id: requested }
                if requested == turn_id && self.agent_config.whip.enabled =>
            {
                let kind = self.register_whip(turn_id, Instant::now());
                self.penalty_responses_remaining = self
                    .penalty_responses_remaining
                    .max(self.agent_config.whip.penalty_completed_responses);
                self.whip_retry_note_pending = Some(turn_id);
                let _ = self
                    .emit(OrchestratorEvent::WhipAcknowledged {
                        conversation_epoch: self.conversation_epoch,
                        turn_id,
                        kind,
                    })
                    .await;
                match kind {
                    WhipKind::Soft => *soft_boundary_from = Some(response_len),
                    WhipKind::Hard => *soft_boundary_from = Some(usize::MAX),
                }
                AwaitedControl::Continue
            }
            OrchestratorCommand::Reset => AwaitedControl::Reset,
            OrchestratorCommand::Shutdown => AwaitedControl::Shutdown,
            OrchestratorCommand::Confirm { .. }
            | OrchestratorCommand::DecidePlan { .. }
            | OrchestratorCommand::DecidePatch { .. }
            | OrchestratorCommand::Rewind { .. }
            | OrchestratorCommand::RefreshSessions { .. }
            | OrchestratorCommand::NewSession { .. }
            | OrchestratorCommand::ResumeSession { .. }
            | OrchestratorCommand::ForkSession { .. }
            | OrchestratorCommand::RenameSession { .. }
            | OrchestratorCommand::SetSessionPinned { .. }
            | OrchestratorCommand::SetSessionArchived { .. }
            | OrchestratorCommand::UpdateRuntimeSettings { .. }
            | OrchestratorCommand::SetDeploymentPricing { .. }
            | OrchestratorCommand::RemoveDeploymentPricing { .. }
            | OrchestratorCommand::GitHubRefresh { .. }
            | OrchestratorCommand::GitHubOpen { .. }
            | OrchestratorCommand::GitHubCheckout { .. }
            | OrchestratorCommand::GitHubCreateDraft { .. }
            | OrchestratorCommand::SetPlanMode { .. }
            | OrchestratorCommand::SetExploreMode { .. }
            | OrchestratorCommand::SetReviewMode { .. }
            | OrchestratorCommand::SetDeepThinkingMode { .. }
            | OrchestratorCommand::SetAutoApprovalPolicy { .. }
            | OrchestratorCommand::SetGoal { .. }
            | OrchestratorCommand::ReloadProjectInstructions { .. }
            | OrchestratorCommand::SetProjectInstructionsEnabled { .. }
            | OrchestratorCommand::SetInstructionSourceEnabled { .. }
            | OrchestratorCommand::ReloadSkills { .. }
            | OrchestratorCommand::SetSkillEnabled { .. }
            | OrchestratorCommand::ReloadAutomation { .. }
            | OrchestratorCommand::SetHookEnabled { .. }
            | OrchestratorCommand::RefreshPlugins { .. }
            | OrchestratorCommand::AddPluginMarketplace { .. }
            | OrchestratorCommand::RemovePluginMarketplace { .. }
            | OrchestratorCommand::InstallLocalPlugin { .. }
            | OrchestratorCommand::InstallMarketplacePlugin { .. }
            | OrchestratorCommand::UpdatePlugin { .. }
            | OrchestratorCommand::SetPluginEnabled { .. }
            | OrchestratorCommand::RemovePlugin { .. }
            | OrchestratorCommand::McpConnect { .. }
            | OrchestratorCommand::McpDisconnect { .. }
            | OrchestratorCommand::McpSetEnabled { .. }
            | OrchestratorCommand::McpAddServer { .. }
            | OrchestratorCommand::SetSubagentMcpAccess { .. }
            | OrchestratorCommand::McpBeginOAuth { .. }
            | OrchestratorCommand::McpPollOAuth { .. }
            | OrchestratorCommand::McpForgetOAuth { .. }
            | OrchestratorCommand::LspConnect { .. }
            | OrchestratorCommand::LspDisconnect { .. }
            | OrchestratorCommand::LspSetEnabled { .. }
            | OrchestratorCommand::LspAddServer { .. }
            | OrchestratorCommand::LspRefresh { .. }
            | OrchestratorCommand::CodeIndexRefresh { .. }
            | OrchestratorCommand::CodeIndexCancel { .. }
            | OrchestratorCommand::CodeIndexPoll { .. }
            | OrchestratorCommand::CodeIndexSearch { .. }
            | OrchestratorCommand::ReloadPrivacy { .. }
            | OrchestratorCommand::RevokeSessionShellGrant { .. }
            | OrchestratorCommand::ClearSessionShellGrants { .. }
            | OrchestratorCommand::AskSideQuestion { .. }
            | OrchestratorCommand::CancelSideQuestion { .. }
            | OrchestratorCommand::EnqueueFollowUp { .. }
            | OrchestratorCommand::EditFollowUp { .. }
            | OrchestratorCommand::CancelFollowUp { .. }
            | OrchestratorCommand::RetryFollowUp { .. }
            | OrchestratorCommand::DispatchFollowUpQueue { .. }
            | OrchestratorCommand::DecideReviewFinding { .. }
            | OrchestratorCommand::SpawnSubagent { .. }
            | OrchestratorCommand::ReloadSubagentProfiles
            | OrchestratorCommand::MessageSubagent { .. }
            | OrchestratorCommand::CancelSubagent { .. }
            | OrchestratorCommand::ResumeSubagent { .. }
            | OrchestratorCommand::AbandonSubagentRecovery { .. }
            | OrchestratorCommand::DecideSubagentCommand { .. }
            | OrchestratorCommand::DecideSubagentBudget { .. }
            | OrchestratorCommand::OpenSubagentReview { .. }
            | OrchestratorCommand::DecideSubagentFile { .. }
            | OrchestratorCommand::Whip { .. }
            | OrchestratorCommand::Interrupt { .. }
            | OrchestratorCommand::ContinueToolLoop { .. }
            | OrchestratorCommand::RetryTurn { .. }
            | OrchestratorCommand::AbortTurn { .. } => AwaitedControl::Continue,
        }
    }

    async fn drain_urgent_stream_controls(
        &mut self,
        turn_id: TurnId,
        response_len: usize,
        soft_boundary_from: &mut Option<usize>,
    ) -> AwaitedControl {
        let mut decision = AwaitedControl::Continue;
        for signal in self.urgent_control.drain() {
            match signal.kind {
                UrgentControlKind::Shutdown => return AwaitedControl::Shutdown,
                UrgentControlKind::Reset => return AwaitedControl::Reset,
                UrgentControlKind::Interrupt { turn_id: requested } if requested == turn_id => {
                    decision = AwaitedControl::Interrupt;
                }
                UrgentControlKind::Whip { turn_id: requested }
                    if requested == turn_id
                        && self.agent_config.whip.enabled
                        && decision == AwaitedControl::Continue =>
                {
                    let kind = self.register_whip(turn_id, signal.timestamp);
                    self.penalty_responses_remaining = self
                        .penalty_responses_remaining
                        .max(self.agent_config.whip.penalty_completed_responses);
                    self.whip_retry_note_pending = Some(turn_id);
                    let _ = self
                        .emit(OrchestratorEvent::WhipAcknowledged {
                            conversation_epoch: self.conversation_epoch,
                            turn_id,
                            kind,
                        })
                        .await;
                    match kind {
                        WhipKind::Soft if *soft_boundary_from != Some(usize::MAX) => {
                            *soft_boundary_from = Some(response_len);
                        }
                        WhipKind::Hard => *soft_boundary_from = Some(usize::MAX),
                        WhipKind::Soft => {}
                    }
                }
                UrgentControlKind::Interrupt { .. } | UrgentControlKind::Whip { .. } => {}
            }
        }
        decision
    }

    fn drain_urgent_busy_controls(&self, turn_id: TurnId) -> AwaitedControl {
        let mut decision = AwaitedControl::Continue;
        for signal in self.urgent_control.drain() {
            match signal.kind {
                UrgentControlKind::Shutdown => return AwaitedControl::Shutdown,
                UrgentControlKind::Reset => return AwaitedControl::Reset,
                UrgentControlKind::Interrupt { turn_id: requested } if requested == turn_id => {
                    decision = AwaitedControl::Interrupt;
                }
                UrgentControlKind::Interrupt { .. } | UrgentControlKind::Whip { .. } => {}
            }
        }
        decision
    }

    #[cfg(test)]
    async fn drain_stream_controls(
        &mut self,
        turn_id: TurnId,
        response_len: usize,
        soft_boundary_from: &mut Option<usize>,
    ) -> AwaitedControl {
        let urgent = self
            .drain_urgent_stream_controls(turn_id, response_len, soft_boundary_from)
            .await;
        if urgent != AwaitedControl::Continue {
            return urgent;
        }
        loop {
            match self.command_rx.try_recv() {
                Ok(command) => {
                    let result = self
                        .handle_stream_control(command, turn_id, response_len, soft_boundary_from)
                        .await;
                    if result != AwaitedControl::Continue {
                        return result;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    return AwaitedControl::Continue;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return AwaitedControl::Shutdown;
                }
            }
        }
    }

    async fn wait_before_network_retry(
        &mut self,
        turn_id: TurnId,
        error: &ApiError,
        attempt: u32,
        turn_cancel: &CancellationToken,
    ) -> RetryWaitExit {
        if !self.set_phase(Some(turn_id), AgentPhase::Requesting).await {
            return RetryWaitExit::Shutdown;
        }

        let wait_cancel = turn_cancel.child_token();
        let client = Arc::clone(&self.client);
        let wait = client.wait_before_retry(error, attempt, &wait_cancel);
        tokio::pin!(wait);
        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    let mut marker = None;
                    match self
                        .drain_urgent_stream_controls(turn_id, 0, &mut marker)
                        .await
                    {
                        AwaitedControl::Continue if marker.is_some() => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Ready;
                        }
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Interrupted;
                        }
                        AwaitedControl::Reset => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Reset;
                        }
                        AwaitedControl::Shutdown => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Shutdown;
                        }
                    }
                }
                result = &mut wait => {
                    return match result {
                        Ok(()) => RetryWaitExit::Ready,
                        Err(ApiError::Cancelled) if wait_cancel.is_cancelled() => {
                            RetryWaitExit::Ready
                        }
                        Err(_) => RetryWaitExit::Ready,
                    };
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        wait_cancel.cancel();
                        return RetryWaitExit::Shutdown;
                    };
                    let mut marker = None;
                    match self
                        .handle_stream_control(
                            command,
                            turn_id,
                            0,
                            &mut marker,
                        )
                        .await
                    {
                        AwaitedControl::Continue => {
                            if marker.is_some() {
                                wait_cancel.cancel();
                                return RetryWaitExit::Ready;
                            }
                        }
                        AwaitedControl::Interrupt => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Interrupted;
                        }
                        AwaitedControl::Reset => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Reset;
                        }
                        AwaitedControl::Shutdown => {
                            wait_cancel.cancel();
                            return RetryWaitExit::Shutdown;
                        }
                    }
                }
            }
        }
    }

    async fn parse_with_controls(
        &mut self,
        turn_id: TurnId,
        source: String,
        turn_cancel: &CancellationToken,
    ) -> Result<Vec<ParserEvent>, TurnExit> {
        let mut worker = tokio::task::spawn_blocking(move || parse_turn(&source));
        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            worker.abort();
                            return Err(TurnExit::Interrupted);
                        }
                        AwaitedControl::Reset => {
                            worker.abort();
                            return Err(TurnExit::Reset);
                        }
                        AwaitedControl::Shutdown => {
                            worker.abort();
                            return Err(TurnExit::Shutdown);
                        }
                    }
                }
                _ = turn_cancel.cancelled() => {
                    worker.abort();
                    return Err(TurnExit::Interrupted);
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        worker.abort();
                        return Err(TurnExit::Shutdown);
                    };
                    match self.handle_busy_control(command, turn_id).await {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            worker.abort();
                            return Err(TurnExit::Interrupted);
                        }
                        AwaitedControl::Reset => {
                            worker.abort();
                            return Err(TurnExit::Reset);
                        }
                        AwaitedControl::Shutdown => {
                            worker.abort();
                            return Err(TurnExit::Shutdown);
                        }
                    }
                }
                parsed = &mut worker => {
                    return parsed.map_err(|error| {
                        TurnExit::Failed(format!("parser worker failed: {error}"))
                    });
                }
            }
        }
    }

    async fn run_read_batch(
        &mut self,
        turn_id: TurnId,
        actions: Vec<ToolAction>,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        if actions.len() < 2
            || actions.len() > MAX_PARALLEL_READ_ACTIONS
            || actions
                .iter()
                .any(|action| !is_parallel_read_action(action))
        {
            return Err(TurnExit::Failed(
                "invalid parallel read batch reached the executor".to_owned(),
            ));
        }

        let mut items = Vec::with_capacity(actions.len());
        for action in actions {
            let action_id = self.allocate_action_id();
            let pre_hooks = self
                .run_hook_event(
                    HookEvent::PreToolUse,
                    Some(action.tool_name()),
                    serde_json::json!({
                        "event": "pre_tool_use",
                        "turn_id": turn_id,
                        "action_id": action_id,
                        "tool": action.tool_name(),
                        "action": &action,
                        "parallel_read_batch": true,
                    }),
                    turn_cancel,
                )
                .await;
            self.publish_hook_notes(&pre_hooks);
            let outcome = match pre_hooks.disposition {
                HookDisposition::Continue => None,
                HookDisposition::Deny { hook_id, message } => Some(ToolOutcome::failure(format!(
                    "blocked by hook {hook_id}: {message}"
                ))),
            };
            items.push(ReadBatchItem {
                action_id,
                action,
                outcome,
            });
        }

        match self.drain_busy_controls(turn_id).await {
            AwaitedControl::Continue => {}
            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
            AwaitedControl::Reset => return Err(TurnExit::Reset),
            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
        }

        let jobs = items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.outcome.is_none())
            .map(|(index, item)| {
                (
                    index,
                    item.action.clone(),
                    self.approval_binding(turn_id, item.action_id, &item.action),
                )
            })
            .collect::<Vec<_>>();
        if jobs.is_empty() {
            return self
                .finish_read_batch(turn_id, items, Vec::new(), turn_cancel)
                .await;
        }

        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
        {
            return Err(TurnExit::Shutdown);
        }
        for item in items.iter().filter(|item| item.outcome.is_none()) {
            if !self
                .emit(OrchestratorEvent::ToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id: item.action_id,
                    action: item.action.clone(),
                })
                .await
            {
                return Err(TurnExit::Shutdown);
            }
        }

        let batch_cancel = turn_cancel.child_token();
        let worker_cancel = batch_cancel.clone();
        let runner = Arc::clone(&self.tool_runner);
        let timeout = self.agent_config.exec_timeout;
        let worker = async move {
            join_all(jobs.into_iter().map(|(index, action, binding)| {
                let runner = Arc::clone(&runner);
                let cancel = worker_cancel.child_token();
                async move {
                    let outcome = runner
                        .execute_action_bound_with_timeout(&action, None, binding, timeout, cancel)
                        .await;
                    (index, outcome)
                }
            }))
            .await
        };
        tokio::pin!(worker);
        let urgent = self.urgent_control.clone();

        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    let exit = match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => continue,
                        AwaitedControl::Interrupt => TurnExit::Interrupted,
                        AwaitedControl::Reset => TurnExit::Reset,
                        AwaitedControl::Shutdown => TurnExit::Shutdown,
                    };
                    batch_cancel.cancel();
                    let results = worker.await;
                    let _ = self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                    return Err(exit);
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        batch_cancel.cancel();
                        let results = worker.await;
                        let _ = self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                        return Err(TurnExit::Shutdown);
                    };
                    match self.handle_busy_control(command, turn_id).await {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            batch_cancel.cancel();
                            let results = worker.await;
                            let _ = self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                            return Err(TurnExit::Interrupted);
                        }
                        AwaitedControl::Reset => {
                            batch_cancel.cancel();
                            let results = worker.await;
                            let _ = self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                            return Err(TurnExit::Reset);
                        }
                        AwaitedControl::Shutdown => {
                            batch_cancel.cancel();
                            let results = worker.await;
                            let _ = self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                            return Err(TurnExit::Shutdown);
                        }
                    }
                }
                results = &mut worker => {
                    return self.finish_read_batch(turn_id, items, results, turn_cancel).await;
                }
            }
        }
    }

    fn read_batch_has_no_hooks(&self, actions: &[ToolAction]) -> bool {
        let Ok(catalog) = self.automation.lock() else {
            return false;
        };
        actions.iter().all(|action| {
            catalog
                .matching_hooks(HookEvent::PreToolUse, Some(action.tool_name()))
                .is_empty()
                && catalog
                    .matching_hooks(HookEvent::PostToolUse, Some(action.tool_name()))
                    .is_empty()
        })
    }

    async fn finish_read_batch(
        &mut self,
        turn_id: TurnId,
        mut items: Vec<ReadBatchItem>,
        results: Vec<(usize, ToolOutcome)>,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        for (index, outcome) in results {
            if let Some(item) = items.get_mut(index) {
                item.outcome = Some(outcome);
            } else {
                tracing::error!(
                    index,
                    "parallel read worker returned an invalid result index"
                );
            }
        }
        for item in items {
            let outcome = item.outcome.unwrap_or_else(|| {
                ToolOutcome::failure(
                    "parallel read worker returned no result; the action was not trusted as complete",
                )
            });
            self.finish_action(turn_id, item.action_id, item.action, outcome, turn_cancel)
                .await?;
        }
        Ok(())
    }

    async fn run_action(
        &mut self,
        turn_id: TurnId,
        action: ToolAction,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        if self.state.work_modes.read_only() && action.is_mutating() {
            let outcome = ToolOutcome::failure(format!(
                "{} is blocked by the active Explore/Review harness-enforced read-only policy; disable every read-only mode before requesting workspace changes or shell commands",
                action.tool_name()
            ));
            self.finish_action_without_hooks(turn_id, action_id, action, outcome)
                .await?;
            return Ok(());
        }
        let pre_hooks = self
            .run_hook_event(
                HookEvent::PreToolUse,
                Some(action.tool_name()),
                serde_json::json!({
                    "event": "pre_tool_use",
                    "turn_id": turn_id,
                    "action_id": action_id,
                    "tool": action.tool_name(),
                    "action": &action,
                }),
                turn_cancel,
            )
            .await;
        self.publish_hook_notes(&pre_hooks);
        if let HookDisposition::Deny { hook_id, message } = pre_hooks.disposition {
            let outcome = ToolOutcome::failure(format!("blocked by hook {hook_id}: {message}"));
            self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                .await?;
            return Ok(());
        }
        let mut reviewed_write_baseline = None;
        let (action, patch_approval) = if let ToolAction::ApplyPatch {
            path,
            search,
            replace,
        } = &action
        {
            if let Err(error) = self.tool_runner.validate_model_file_path(path) {
                let outcome = ToolOutcome::failure(format!(
                    "apply_patch cannot be reviewed safely before execution: {error}"
                ));
                self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            }
            let review = Arc::new(PatchReview::new(path, search, replace));
            if review.hunks.is_empty() {
                let outcome = ToolOutcome::failure(
                    "apply_patch contains no changes; search and replace are identical",
                );
                self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            }
            let original_action = action.clone();
            let Some(selection) = self
                .await_patch_approval(turn_id, action_id, Arc::clone(&review), turn_cancel)
                .await?
            else {
                let outcome = ToolOutcome::declined(original_action.clone());
                self.finish_action(turn_id, action_id, original_action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            };
            let summary = PatchApprovalSummary {
                approved_hunks: selection.approved_hunks,
                total_hunks: selection.total_hunks,
            };
            (
                ToolAction::ApplyPatch {
                    path: path.clone(),
                    search: search.clone(),
                    replace: selection.replacement,
                },
                Some(summary),
            )
        } else if let ToolAction::WriteFile { path, content } = &action {
            let baseline = match self
                .tool_runner
                .capture_write_file_baseline(path, turn_cancel.child_token())
                .await
            {
                Ok(baseline) => baseline,
                Err(error) => {
                    let outcome = ToolOutcome::failure(format!(
                        "write_file cannot be reviewed safely before execution: {error}"
                    ));
                    self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                        .await?;
                    return Ok(());
                }
            };
            let original = match &baseline {
                ReviewedWriteBaseline::Missing => "",
                ReviewedWriteBaseline::Existing(original) => original.as_str(),
            };
            let review = Arc::new(PatchReview::new(path, original, content));
            if review.hunks.is_empty() {
                let outcome = ToolOutcome::failure(
                    "write_file contains no changes compared with the current destination",
                );
                self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            }
            let original_action = action.clone();
            let Some(selection) = self
                .await_patch_approval(turn_id, action_id, Arc::clone(&review), turn_cancel)
                .await?
            else {
                let outcome = ToolOutcome::declined(original_action.clone());
                self.finish_action(turn_id, action_id, original_action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            };
            let summary = PatchApprovalSummary {
                approved_hunks: selection.approved_hunks,
                total_hunks: selection.total_hunks,
            };
            reviewed_write_baseline = Some(baseline);
            (
                ToolAction::WriteFile {
                    path: path.clone(),
                    content: selection.replacement,
                },
                Some(summary),
            )
        } else {
            (action, None)
        };
        let binding = self.approval_binding(turn_id, action_id, &action);
        let confirmation_decision = self.tool_runner.action_confirmation_decision(&action);
        let approval = if confirmation_decision.requires_confirmation() {
            let ToolAction::ExecuteCommand {
                command,
                requires_confirmation,
            } = &action
            else {
                let outcome = ToolOutcome::failure(
                    "internal confirmation policy error: non-shell action required approval",
                );
                self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                    .await?;
                return Ok(());
            };
            if self
                .session_shell_permissions
                .authorizes(command, confirmation_decision)
            {
                Some(CommandApproval::confirmed_for_bound(
                    command,
                    *requires_confirmation,
                    binding,
                ))
            } else {
                match self
                    .await_confirmation(
                        turn_id,
                        action_id,
                        &action,
                        binding,
                        confirmation_decision,
                        turn_cancel,
                    )
                    .await?
                {
                    Some(approval) => Some(approval),
                    None => {
                        let outcome = ToolOutcome::declined(action.clone());
                        self.finish_action(turn_id, action_id, action, outcome, turn_cancel)
                            .await?;
                        return Ok(());
                    }
                }
            }
        } else {
            None
        };

        match self.drain_busy_controls(turn_id).await {
            AwaitedControl::Continue => {}
            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
            AwaitedControl::Reset => return Err(TurnExit::Reset),
            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
        }
        let mut checkpoint_before = self.begin_action_checkpoint(&action).await?;

        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::ToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    action: action.clone(),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let action_cancel = turn_cancel.child_token();
        let runner = Arc::clone(&self.tool_runner);
        let action_for_worker = action.clone();
        let reviewed_write_for_worker = reviewed_write_baseline;
        let worker_cancel = action_cancel.clone();
        let action_timeout = match &action {
            ToolAction::ExecuteCommand { command, .. } => self
                .agent_config
                .shell
                .timeout_for(command, self.agent_config.exec_timeout),
            _ => self.agent_config.exec_timeout,
        };
        let worker = async move {
            if let Some(baseline) = reviewed_write_for_worker {
                runner
                    .execute_reviewed_write_bound_with_timeout(
                        &action_for_worker,
                        binding,
                        action_timeout,
                        baseline,
                        worker_cancel,
                    )
                    .await
            } else {
                runner
                    .execute_action_bound_with_timeout(
                        &action_for_worker,
                        approval,
                        binding,
                        action_timeout,
                        worker_cancel,
                    )
                    .await
            }
        };
        tokio::pin!(worker);
        let urgent = self.urgent_control.clone();

        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Interrupted);
                        }
                        AwaitedControl::Reset => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Reset);
                        }
                        AwaitedControl::Shutdown => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Shutdown);
                        }
                    }
                }
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        action_cancel.cancel();
                        let outcome = worker.await;
                        let _ = self.finish_checkpointed_action(
                            turn_id,
                            action_id,
                            action,
                            outcome,
                            ActionFinalization {
                                checkpoint_before: checkpoint_before.take(),
                                patch_approval,
                                hook_cancel: turn_cancel,
                            },
                        ).await;
                        return Err(TurnExit::Shutdown);
                    };
                    match self.handle_busy_control(command, turn_id).await {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Interrupted);
                        }
                        AwaitedControl::Reset => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Reset);
                        }
                        AwaitedControl::Shutdown => {
                            action_cancel.cancel();
                            let outcome = worker.await;
                            let _ = self.finish_checkpointed_action(
                                turn_id,
                                action_id,
                                action,
                                outcome,
                                ActionFinalization {
                                    checkpoint_before: checkpoint_before.take(),
                                    patch_approval,
                                    hook_cancel: turn_cancel,
                                },
                            ).await;
                            return Err(TurnExit::Shutdown);
                        }
                    }
                }
                outcome = &mut worker => {
                    return self
                        .finish_checkpointed_action(
                            turn_id,
                            action_id,
                            action,
                            outcome,
                            ActionFinalization {
                                checkpoint_before: checkpoint_before.take(),
                                patch_approval,
                                hook_cancel: turn_cancel,
                            },
                        )
                        .await;
                }
            }
        }
    }

    async fn run_subagent_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let arguments = match serde_json::from_str::<Value>(&native.arguments) {
            Ok(Value::Object(arguments)) => Value::Object(arguments),
            Ok(_) => Value::Null,
            Err(_) => Value::Null,
        };
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name.clone(),
            server: "builtin:subagents".to_owned(),
            tool: native.name,
            arguments,
        });

        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let result = self
            .execute_subagent_function(&call.function_name, &call.arguments, turn_cancel)
            .await;
        let (content, status, is_error) = match result {
            Ok(content) => (content, ToolResultStatus::Success, false),
            Err(error) => (error.to_string(), ToolResultStatus::Failure, true),
        };
        self.finish_mcp_call(
            turn_id,
            action_id,
            call,
            McpCallOutput {
                content,
                is_error,
                truncated: false,
            },
            status,
        )
        .await
    }

    async fn execute_subagent_function(
        &self,
        name: &str,
        arguments: &Value,
        cancel: &CancellationToken,
    ) -> Result<String, SubagentError> {
        match name {
            SPAWN_AGENT_TOOL => {
                let arguments: SpawnAgentArguments = parse_native_arguments(arguments)?;
                let id = self
                    .subagents
                    .spawn(SpawnSubagentRequest {
                        task: arguments.task,
                        profile_id: arguments.profile_id,
                        session_id: self.current_session_id.as_ref().map(ToString::to_string),
                        deployment: self.deployment.clone(),
                        reasoning_effort: self.base_reasoning_effort,
                        instructions: self.effective_instructions(),
                        dependencies: arguments
                            .depends_on
                            .into_iter()
                            .map(SubagentId::new)
                            .collect(),
                        file_claims: arguments.file_claims,
                    })
                    .await?;
                let snapshot = self.subagents.agent_snapshot(id)?;
                Ok(subagent_snapshot_json(&snapshot, false).to_string())
            }
            LIST_AGENTS_TOOL => {
                let _: EmptyNativeArguments = parse_native_arguments(arguments)?;
                let fleet = self.subagents.snapshot();
                let agents = fleet
                    .agents
                    .iter()
                    .map(|agent| subagent_snapshot_json(agent, false))
                    .collect::<Vec<_>>();
                Ok(serde_json::json!({
                    "revision": fleet.revision,
                    "enabled": fleet.enabled,
                    "capacity": fleet.capacity,
                    "active": fleet.active,
                    "availability_error": fleet.availability_error,
                    "agents": agents,
                })
                .to_string())
            }
            GET_AGENT_TOOL => {
                let arguments: AgentRevisionArguments = parse_native_arguments(arguments)?;
                let id = SubagentId::new(arguments.agent_id);
                let snapshot = self.subagents.agent_snapshot(id)?;
                if snapshot.revision != arguments.revision {
                    return Err(SubagentError::Stale { id });
                }
                Ok(subagent_snapshot_json(&snapshot, true).to_string())
            }
            SEND_AGENT_MESSAGE_TOOL => {
                let arguments: AgentMessageArguments = parse_native_arguments(arguments)?;
                let id = SubagentId::new(arguments.agent_id);
                self.subagents
                    .send_message(id, arguments.revision, arguments.message)?;
                Ok(serde_json::json!({ "accepted": true, "agent_id": id.get() }).to_string())
            }
            INTERRUPT_AGENT_TOOL => {
                let arguments: AgentRevisionArguments = parse_native_arguments(arguments)?;
                let id = SubagentId::new(arguments.agent_id);
                self.subagents.cancel(id, arguments.revision)?;
                Ok(serde_json::json!({ "accepted": true, "agent_id": id.get() }).to_string())
            }
            WAIT_AGENT_TOOL => {
                let arguments: WaitAgentArguments = parse_native_arguments(arguments)?;
                let wait = Duration::from_millis(arguments.timeout_ms).min(SUBAGENT_WAIT_MAX);
                let snapshot = self
                    .subagents
                    .wait_for_update(
                        SubagentId::new(arguments.agent_id),
                        arguments.revision,
                        wait,
                        cancel,
                    )
                    .await?;
                Ok(subagent_snapshot_json(&snapshot, true).to_string())
            }
            _ => Err(SubagentError::Protocol(format!(
                "unknown built-in sub-agent function {name:?}"
            ))),
        }
    }

    async fn run_mcp_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        if self.state.work_modes.read_only() && !explore_allows_native_function(&native.name) {
            let action_id = self.allocate_action_id();
            let arguments = serde_json::from_str::<Value>(&native.arguments).unwrap_or(Value::Null);
            let call = Arc::new(McpToolCall {
                call_id: native.call_id,
                function_name: native.name.clone(),
                server: "builtin:read-only-policy".to_owned(),
                tool: native.name,
                arguments,
            });
            return self
                .finish_mcp_call(
                    turn_id,
                    action_id,
                    call,
                    McpCallOutput {
                        content: "Native MCP and sub-agent calls are blocked by the active Explore/Review harness-enforced read-only policy. LSP, repository intelligence, read-only Skills, update_goal, and the active Review tools remain available."
                            .to_owned(),
                        is_error: true,
                        truncated: false,
                    },
                    ToolResultStatus::Failure,
                )
                .await;
        }
        if native.name == UPDATE_GOAL_TOOL {
            return self.run_goal_update(turn_id, native).await;
        }
        if matches!(native.name.as_str(), REVIEW_DIFF_TOOL | SUBMIT_REVIEW_TOOL) {
            return self.run_review_call(turn_id, native).await;
        }
        if is_skill_function(&native.name) {
            return self.run_skill_call(turn_id, native, turn_cancel).await;
        }
        if is_subagent_function(&native.name) {
            return self.run_subagent_call(turn_id, native, turn_cancel).await;
        }
        if is_lsp_function(&native.name) {
            return self.run_lsp_call(turn_id, native, turn_cancel).await;
        }
        if is_code_index_function(&native.name) {
            return self.run_code_index_call(turn_id, native, turn_cancel).await;
        }
        let action_id = self.allocate_action_id();
        let arguments = match serde_json::from_str::<Value>(&native.arguments) {
            Ok(Value::Object(arguments)) => Value::Object(arguments),
            Ok(_) => {
                let call = Arc::new(McpToolCall {
                    call_id: native.call_id,
                    function_name: native.name.clone(),
                    server: "<unresolved>".to_owned(),
                    tool: native.name,
                    arguments: Value::Null,
                });
                return self
                    .finish_mcp_call(
                        turn_id,
                        action_id,
                        call,
                        McpCallOutput {
                            content: "Native function arguments must be a JSON object.".to_owned(),
                            is_error: true,
                            truncated: false,
                        },
                        ToolResultStatus::Failure,
                    )
                    .await;
            }
            Err(error) => {
                let call = Arc::new(McpToolCall {
                    call_id: native.call_id,
                    function_name: native.name.clone(),
                    server: "<unresolved>".to_owned(),
                    tool: native.name,
                    arguments: Value::Null,
                });
                return self
                    .finish_mcp_call(
                        turn_id,
                        action_id,
                        call,
                        McpCallOutput {
                            content: format!("Native function arguments are invalid JSON: {error}"),
                            is_error: true,
                            truncated: false,
                        },
                        ToolResultStatus::Failure,
                    )
                    .await;
            }
        };

        let tool = match self
            .mcp
            .as_ref()
            .ok_or_else(|| "MCP is disabled".to_owned())
            .and_then(|manager| {
                manager
                    .tool(&native.name)
                    .map_err(|error| error.to_string())
            }) {
            Ok(tool) => tool,
            Err(message) => {
                let call = Arc::new(McpToolCall {
                    call_id: native.call_id,
                    function_name: native.name.clone(),
                    server: "<unresolved>".to_owned(),
                    tool: native.name,
                    arguments,
                });
                return self
                    .finish_mcp_call(
                        turn_id,
                        action_id,
                        call,
                        McpCallOutput {
                            content: message,
                            is_error: true,
                            truncated: false,
                        },
                        ToolResultStatus::Failure,
                    )
                    .await;
            }
        };
        let mcp_read_only = tool.read_only_hint == Some(true)
            && tool.destructive_hint != Some(true)
            && tool.open_world_hint != Some(true);
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name,
            server: tool.server,
            tool: tool.name,
            arguments,
        });

        let permission = self
            .mcp
            .as_ref()
            .ok_or_else(|| TurnExit::Failed("MCP manager disappeared".to_owned()))?
            .permission_for(&call.function_name);
        let approved = match permission {
            Ok(McpPermissionDecision::Allow) => true,
            Ok(McpPermissionDecision::Deny { reason }) => {
                return self
                    .finish_mcp_call(
                        turn_id,
                        action_id,
                        call,
                        McpCallOutput {
                            content: reason,
                            is_error: true,
                            truncated: false,
                        },
                        ToolResultStatus::Failure,
                    )
                    .await;
            }
            Ok(McpPermissionDecision::RequireApproval { reason }) => {
                if (mcp_read_only && self.state.auto_approval.mcp_read_only)
                    || (!mcp_read_only && self.state.auto_approval.mcp_mutating)
                {
                    self.snapshot_tx.send_modify(|snapshot| {
                        snapshot.status = format!(
                            "MCP {}::{} auto-approved by session policy",
                            call.server, call.tool
                        );
                    });
                    true
                } else {
                    self.await_mcp_confirmation(
                        turn_id,
                        action_id,
                        Arc::clone(&call),
                        reason,
                        turn_cancel,
                    )
                    .await?
                }
            }
            Err(error) => {
                return self
                    .finish_mcp_call(
                        turn_id,
                        action_id,
                        call,
                        McpCallOutput {
                            content: error.to_string(),
                            is_error: true,
                            truncated: false,
                        },
                        ToolResultStatus::Failure,
                    )
                    .await;
            }
        };
        if !approved {
            return self
                .finish_mcp_call(
                    turn_id,
                    action_id,
                    call,
                    McpCallOutput {
                        content: "The user declined this MCP tool call.".to_owned(),
                        is_error: true,
                        truncated: false,
                    },
                    ToolResultStatus::Declined,
                )
                .await;
        }

        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }
        let manager = self
            .mcp
            .as_ref()
            .ok_or_else(|| TurnExit::Failed("MCP manager disappeared".to_owned()))?;
        let result = tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
            result = manager.call(&call.function_name, call.arguments.clone(), true) => result,
        };
        let outcome = match result {
            Ok(output) => output,
            Err(error) => McpCallOutput {
                content: error.to_string(),
                is_error: true,
                truncated: false,
            },
        };
        let status = if outcome.is_error {
            ToolResultStatus::Failure
        } else {
            ToolResultStatus::Success
        };
        self.finish_mcp_call(turn_id, action_id, call, outcome, status)
            .await
    }

    async fn run_skill_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let arguments = serde_json::from_str::<Value>(&native.arguments).unwrap_or(Value::Null);
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name.clone(),
            server: "builtin:skills".to_owned(),
            tool: native.name,
            arguments,
        });
        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let catalog = self.skills.clone();
        let function_name = call.function_name.clone();
        let raw_arguments = native.arguments;
        let operation = tokio::task::spawn_blocking(move || -> Result<String, SkillCallError> {
            match function_name.as_str() {
                READ_SKILL_TOOL => {
                    let arguments = serde_json::from_str::<SkillIdArguments>(&raw_arguments)
                        .map_err(|source| SkillCallError::Arguments {
                            function: READ_SKILL_TOOL,
                            source,
                        })?;
                    let content = catalog.read_skill(&arguments.skill_id)?;
                    Ok(serde_json::to_string(&content)?)
                }
                LIST_SKILL_RESOURCES_TOOL => {
                    let arguments = serde_json::from_str::<SkillIdArguments>(&raw_arguments)
                        .map_err(|source| SkillCallError::Arguments {
                            function: LIST_SKILL_RESOURCES_TOOL,
                            source,
                        })?;
                    let resources = catalog.list_resources(&arguments.skill_id)?;
                    Ok(serde_json::to_string(&resources)?)
                }
                READ_SKILL_RESOURCE_TOOL => {
                    let arguments = serde_json::from_str::<SkillResourceArguments>(&raw_arguments)
                        .map_err(|source| SkillCallError::Arguments {
                            function: READ_SKILL_RESOURCE_TOOL,
                            source,
                        })?;
                    let resource = catalog.read_resource(&arguments.skill_id, &arguments.path)?;
                    Ok(serde_json::to_string(&resource)?)
                }
                _ => Err(SkillCallError::UnknownFunction {
                    function: function_name,
                }),
            }
        });
        let result = tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
            result = operation => result.map_err(SkillCallError::from).and_then(|result| result),
        };
        let (outcome, status) = match result {
            Ok(content) => (
                McpCallOutput {
                    content,
                    is_error: false,
                    truncated: false,
                },
                ToolResultStatus::Success,
            ),
            Err(error) => (
                McpCallOutput {
                    content: error.to_string(),
                    is_error: true,
                    truncated: false,
                },
                ToolResultStatus::Failure,
            ),
        };
        self.finish_mcp_call(turn_id, action_id, call, outcome, status)
            .await
    }

    async fn run_review_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let arguments = serde_json::from_str::<Value>(&native.arguments).unwrap_or(Value::Null);
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name.clone(),
            server: "builtin:review".to_owned(),
            tool: native.name,
            arguments,
        });
        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let result = match call.function_name.as_str() {
            REVIEW_DIFF_TOOL => {
                let parsed = serde_json::from_value::<ReviewDiffArguments>(call.arguments.clone())
                    .map_err(|error| format!("invalid review_diff arguments: {error}"));
                match (parsed, self.active_review.as_ref()) {
                    (Ok(arguments), Some(snapshot)) => snapshot
                        .chunk(arguments.offset, arguments.max_bytes)
                        .and_then(|chunk| {
                            serde_json::to_string(&chunk).map_err(|error| {
                                crate::agent::review::ReviewError::InvalidUtf8(error.to_string())
                            })
                        })
                        .map_err(|error| error.to_string()),
                    (Ok(_), None) => Err(
                        "review_diff is unavailable outside an active immutable Review Mode turn"
                            .to_owned(),
                    ),
                    (Err(error), _) => Err(error),
                }
            }
            SUBMIT_REVIEW_TOOL => {
                let parsed =
                    serde_json::from_value::<SubmitReviewArguments>(call.arguments.clone())
                        .map_err(|error| format!("invalid submit_review arguments: {error}"));
                match (parsed, self.active_review.as_ref()) {
                    (Ok(arguments), Some(snapshot)) => self
                        .state
                        .reviews
                        .submit(turn_id, snapshot, arguments)
                        .map(|report| {
                            self.snapshot_tx.send_modify(|snapshot| {
                                snapshot.reviews = self.state.reviews.snapshot();
                                snapshot.status = format!(
                                    "Review #{} submitted: {} finding(s)",
                                    report.id,
                                    report.findings.len()
                                );
                            });
                            serde_json::json!({
                                "accepted": true,
                                "report_id": report.id,
                                "revision": report.revision,
                                "verdict": report.verdict,
                                "findings": report.findings.len(),
                                "snapshot_sha256": report.snapshot_sha256,
                            })
                            .to_string()
                        })
                        .map_err(|error| error.to_string()),
                    (Ok(_), None) => Err(
                        "submit_review is unavailable outside an active immutable Review Mode turn"
                            .to_owned(),
                    ),
                    (Err(error), _) => Err(error),
                }
            }
            _ => Err("unknown built-in review function".to_owned()),
        };
        let (content, status, is_error) = match result {
            Ok(content) => (content, ToolResultStatus::Success, false),
            Err(error) => (error, ToolResultStatus::Failure, true),
        };
        self.finish_mcp_call(
            turn_id,
            action_id,
            call,
            McpCallOutput {
                content,
                is_error,
                truncated: false,
            },
            status,
        )
        .await
    }

    async fn run_lsp_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let arguments = serde_json::from_str::<Value>(&native.arguments).unwrap_or(Value::Null);
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name.clone(),
            server: "builtin:lsp".to_owned(),
            tool: native.name,
            arguments,
        });
        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }
        let result = match &mut self.lsp {
            Some(lsp) => {
                lsp.call(&call.function_name, &native.arguments, turn_cancel)
                    .await
            }
            None => Err(crate::lsp::LspError::RuntimeDisabled),
        };
        let (outcome, status) = match result {
            Ok(output) => (
                McpCallOutput {
                    content: output.content,
                    is_error: false,
                    truncated: output.truncated,
                },
                ToolResultStatus::Success,
            ),
            Err(error) => (
                McpCallOutput {
                    content: error.to_string(),
                    is_error: true,
                    truncated: false,
                },
                ToolResultStatus::Failure,
            ),
        };
        self.publish_lsp_snapshot(None);
        self.finish_mcp_call(turn_id, action_id, call, outcome, status)
            .await
    }

    async fn run_code_index_call(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
        turn_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let arguments = serde_json::from_str::<Value>(&native.arguments).unwrap_or(Value::Null);
        let call = Arc::new(McpToolCall {
            call_id: native.call_id,
            function_name: native.name.clone(),
            server: "builtin:code-index".to_owned(),
            tool: native.name,
            arguments,
        });
        if !self
            .set_phase(Some(turn_id), AgentPhase::ExecutingTools)
            .await
            || !self
                .emit(OrchestratorEvent::McpToolStarted {
                    conversation_epoch: self.conversation_epoch,
                    turn_id,
                    action_id,
                    call: Arc::clone(&call),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }
        let result = match &mut self.code_index {
            Some(index) => {
                index
                    .call(&call.function_name, &native.arguments, turn_cancel)
                    .await
            }
            None => Err(crate::code_index::CodeIndexError::Disabled),
        };
        let (outcome, status) = match result {
            Ok(content) => (
                McpCallOutput {
                    content,
                    is_error: false,
                    truncated: false,
                },
                ToolResultStatus::Success,
            ),
            Err(error) => (
                McpCallOutput {
                    content: error.to_string(),
                    is_error: true,
                    truncated: false,
                },
                ToolResultStatus::Failure,
            ),
        };
        self.publish_code_index_snapshot(None);
        self.finish_mcp_call(turn_id, action_id, call, outcome, status)
            .await
    }

    async fn run_goal_update(
        &mut self,
        turn_id: TurnId,
        native: FunctionCall,
    ) -> Result<(), TurnExit> {
        let action_id = self.allocate_action_id();
        let updated = serde_json::from_str::<GoalUpdate>(&native.arguments)
            .map_err(|error| format!("invalid update_goal arguments: {error}"))
            .and_then(|update| {
                self.state
                    .work_modes
                    .update_goal(turn_id, update)
                    .cloned()
                    .map_err(|error| error.to_string())
            });
        let (status, output, ui_status) = match updated {
            Ok(goal) => (
                ToolResultStatus::Success,
                serde_json::json!({ "ok": true, "goal": goal }).to_string(),
                "Persistent goal progress updated".to_owned(),
            ),
            Err(message) => {
                let ui_status = format!("Persistent goal progress update failed: {message}");
                (
                    ToolResultStatus::Failure,
                    serde_json::json!({ "ok": false, "error": message }).to_string(),
                    ui_status,
                )
            }
        };
        let sequence =
            self.state
                .push_tool_diagnostic(turn_id, action_id, UPDATE_GOAL_TOOL, status, &output);
        let _attached = self.state.set_api_items(
            sequence,
            vec![serde_json::json!({
                "type": "function_call_output",
                "call_id": native.call_id,
                "output": output,
            })],
        );
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.work_modes = self.state.work_modes.clone();
            snapshot.status = ui_status;
        });
        if !self.emit_history().await {
            return Err(TurnExit::Shutdown);
        }
        Ok(())
    }

    async fn await_mcp_confirmation(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
        reason: String,
        turn_cancel: &CancellationToken,
    ) -> Result<bool, TurnExit> {
        if !self
            .set_phase(Some(turn_id), AgentPhase::AwaitingConfirmation)
            .await
            || !self
                .emit(OrchestratorEvent::McpConfirmationRequested {
                    turn_id,
                    action_id,
                    call,
                    reason,
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }
        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => match self.drain_urgent_busy_controls(turn_id) {
                    AwaitedControl::Continue => {}
                    AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                    AwaitedControl::Reset => return Err(TurnExit::Reset),
                    AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                },
                _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        return Err(TurnExit::Shutdown);
                    };
                    match command {
                        OrchestratorCommand::Confirm {
                            turn_id: confirmed_turn,
                            action_id: confirmed_action,
                            decision,
                        } if confirmed_turn == turn_id && confirmed_action == action_id => {
                            return Ok(decision == ShellApprovalDecision::RunOnce);
                        }
                        other => match self.handle_busy_control(other, turn_id).await {
                            AwaitedControl::Continue => {}
                            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                            AwaitedControl::Reset => return Err(TurnExit::Reset),
                            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                        },
                    }
                }
            }
        }
    }

    async fn finish_mcp_call(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        call: Arc<McpToolCall>,
        outcome: McpCallOutput,
        status: ToolResultStatus,
    ) -> Result<(), TurnExit> {
        let output = serde_json::json!({
            "ok": !outcome.is_error,
            "server": call.server,
            "tool": call.tool,
            "content": outcome.content,
            "truncated": outcome.truncated,
        })
        .to_string();
        let sequence = self.state.push_tool_diagnostic(
            turn_id,
            action_id,
            format!("mcp:{}::{}", call.server, call.tool),
            status,
            &output,
        );
        let _attached = self.state.set_api_items(
            sequence,
            vec![serde_json::json!({
                "type": "function_call_output",
                "call_id": call.call_id,
                "output": output,
            })],
        );
        if !self
            .emit(OrchestratorEvent::McpToolCompleted {
                conversation_epoch: self.conversation_epoch,
                turn_id,
                action_id,
                call,
                outcome,
            })
            .await
            || !self.emit_history().await
        {
            return Err(TurnExit::Shutdown);
        }
        Ok(())
    }

    async fn await_patch_approval(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        review: Arc<PatchReview>,
        turn_cancel: &CancellationToken,
    ) -> Result<Option<PatchSelection>, TurnExit> {
        if self.state.auto_approval.workspace_changes {
            let decisions = vec![true; review.hunks.len()];
            return review
                .apply_decisions(&decisions)
                .map(Some)
                .map_err(|error| TurnExit::Failed(error.to_string()));
        }
        if !self
            .set_phase(Some(turn_id), AgentPhase::AwaitingPatchApproval)
            .await
            || !self
                .emit(OrchestratorEvent::PatchApprovalRequested {
                    turn_id,
                    action_id,
                    review: Arc::clone(&review),
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                        AwaitedControl::Reset => return Err(TurnExit::Reset),
                        AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                    }
                }
                _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command_event = self.command_rx.recv() => {
                    let Some(command_event) = command_event else {
                        return Err(TurnExit::Shutdown);
                    };
                    match command_event {
                        OrchestratorCommand::DecidePatch {
                            turn_id: decided_turn,
                            action_id: decided_action,
                            decisions,
                        } if decided_turn == turn_id && decided_action == action_id => {
                            match review.apply_decisions(&decisions) {
                                Ok(selection) if selection.approved_hunks == 0 => return Ok(None),
                                Ok(selection) => return Ok(Some(selection)),
                                Err(error) => {
                                    let _ = self.emit(OrchestratorEvent::BusyRejected {
                                        turn_id,
                                        message: format!(
                                            "Invalid patch approval was ignored (fail closed): {error}"
                                        ),
                                    }).await;
                                }
                            }
                        }
                        other => match self.handle_busy_control(other, turn_id).await {
                            AwaitedControl::Continue => {}
                            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                            AwaitedControl::Reset => return Err(TurnExit::Reset),
                            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                        }
                    }
                }
            }
        }
    }

    async fn await_confirmation(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        action: &ToolAction,
        binding: ApprovalBinding,
        confirmation_decision: ConfirmationDecision,
        turn_cancel: &CancellationToken,
    ) -> Result<Option<CommandApproval>, TurnExit> {
        let ToolAction::ExecuteCommand {
            command,
            requires_confirmation,
        } = action
        else {
            return Ok(None);
        };
        let Some(reason) = confirmation_decision.reason() else {
            return Ok(None);
        };
        if self.state.auto_approval.shell
            && matches!(
                reason,
                ConfirmationReason::PolicyRequired | ConfirmationReason::NotAllowlisted
            )
        {
            self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status = "Shell command auto-approved by the session policy".to_owned();
            });
            return Ok(Some(CommandApproval::confirmed_for_bound(
                command,
                *requires_confirmation,
                binding,
            )));
        }
        let session_trust_available = session_grant_is_eligible(confirmation_decision);

        if !self
            .set_phase(Some(turn_id), AgentPhase::AwaitingConfirmation)
            .await
            || !self
                .emit(OrchestratorEvent::ConfirmationRequested {
                    turn_id,
                    action_id,
                    action: action.clone(),
                    command: command.clone(),
                    command_bytes: command.len(),
                    command_digest: binding.command_digest,
                    model_requested: *requires_confirmation,
                    reason,
                    session_trust_available,
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                        AwaitedControl::Reset => return Err(TurnExit::Reset),
                        AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                    }
                }
                _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command_event = self.command_rx.recv() => {
                    let Some(command_event) = command_event else {
                        return Err(TurnExit::Shutdown);
                    };
                    match command_event {
                        OrchestratorCommand::Confirm {
                            turn_id: confirmed_turn,
                            action_id: confirmed_action,
                            decision,
                        } if confirmed_turn == turn_id
                            && confirmed_action == action_id =>
                        {
                            match decision {
                                ShellApprovalDecision::Decline => return Ok(None),
                                ShellApprovalDecision::RunOnce => {}
                                ShellApprovalDecision::TrustExactForSession
                                    if session_trust_available =>
                                {
                                    self.session_shell_permissions.grant_exact(
                                        command,
                                        binding.command_digest,
                                        turn_id,
                                        action_id,
                                    );
                                    self.publish_shell_permissions(
                                        "Exact command trusted for this session only",
                                    );
                                }
                                ShellApprovalDecision::TrustExactForSession => {
                                    // A stale or forged UI decision can never override a
                                    // model-requested or forced-confirm policy decision.
                                    continue;
                                }
                            }
                            return Ok(Some(CommandApproval::confirmed_for_bound(
                                command,
                                *requires_confirmation,
                                binding,
                            )));
                        }
                        other => match self.handle_busy_control(other, turn_id).await {
                            AwaitedControl::Continue => {}
                            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                            AwaitedControl::Reset => return Err(TurnExit::Reset),
                            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                        }
                    }
                }
            }
        }
    }

    async fn await_continuation(
        &mut self,
        turn_id: TurnId,
        completed_iterations: u32,
        max_iterations: u32,
        turn_cancel: &CancellationToken,
    ) -> Result<bool, TurnExit> {
        if self.state.auto_approval.continuations {
            self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status =
                    "Tool-loop continuation auto-approved by the session policy".to_owned();
            });
            return Ok(true);
        }
        let continuation_id = self.allocate_continuation_id();
        if !self
            .set_phase(Some(turn_id), AgentPhase::AwaitingContinuation)
            .await
            || !self
                .emit(OrchestratorEvent::ContinuationRequested {
                    turn_id,
                    continuation_id,
                    completed_iterations,
                    max_iterations,
                })
                .await
        {
            return Err(TurnExit::Shutdown);
        }

        let urgent = self.urgent_control.clone();
        loop {
            tokio::select! {
                biased;
                _ = urgent.notified() => {
                    match self.drain_urgent_busy_controls(turn_id) {
                        AwaitedControl::Continue => {}
                        AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                        AwaitedControl::Reset => return Err(TurnExit::Reset),
                        AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                    }
                }
                _ = turn_cancel.cancelled() => return Err(TurnExit::Interrupted),
                result = self.side_result_rx.recv() => {
                    if let Some(result) = result {
                        self.finish_side_question(result).await;
                    }
                }
                command = self.command_rx.recv() => {
                    let Some(command) = command else {
                        return Err(TurnExit::Shutdown);
                    };
                    match command {
                        OrchestratorCommand::ContinueToolLoop {
                            turn_id: requested,
                            continuation_id: requested_continuation,
                            continue_loop,
                        } if requested == turn_id && requested_continuation == continuation_id => {
                            return Ok(continue_loop);
                        }
                        other => match self.handle_busy_control(other, turn_id).await {
                            AwaitedControl::Continue => {}
                            AwaitedControl::Interrupt => return Err(TurnExit::Interrupted),
                            AwaitedControl::Reset => return Err(TurnExit::Reset),
                            AwaitedControl::Shutdown => return Err(TurnExit::Shutdown),
                        }
                    }
                }
            }
        }
    }

    async fn handle_busy_control(
        &mut self,
        command: OrchestratorCommand,
        turn_id: TurnId,
    ) -> AwaitedControl {
        let Some(command) = self.handle_side_chat_command(command).await else {
            return AwaitedControl::Continue;
        };
        let Some(command) = self.handle_follow_up_command(command).await else {
            return AwaitedControl::Continue;
        };
        let Some(command) = self.handle_subagent_command(command).await else {
            return AwaitedControl::Continue;
        };
        match command {
            OrchestratorCommand::Submit { .. } => {
                let _ = self
                    .emit(OrchestratorEvent::BusyRejected {
                        turn_id,
                        message: "A turn is already in progress.".to_owned(),
                    })
                    .await;
                AwaitedControl::Continue
            }
            OrchestratorCommand::Interrupt { turn_id: requested } if requested == turn_id => {
                AwaitedControl::Interrupt
            }
            OrchestratorCommand::Reset => AwaitedControl::Reset,
            OrchestratorCommand::Shutdown => AwaitedControl::Shutdown,
            OrchestratorCommand::Confirm { .. }
            | OrchestratorCommand::DecidePlan { .. }
            | OrchestratorCommand::DecidePatch { .. }
            | OrchestratorCommand::Rewind { .. }
            | OrchestratorCommand::RefreshSessions { .. }
            | OrchestratorCommand::NewSession { .. }
            | OrchestratorCommand::ResumeSession { .. }
            | OrchestratorCommand::ForkSession { .. }
            | OrchestratorCommand::RenameSession { .. }
            | OrchestratorCommand::SetSessionPinned { .. }
            | OrchestratorCommand::SetSessionArchived { .. }
            | OrchestratorCommand::UpdateRuntimeSettings { .. }
            | OrchestratorCommand::SetDeploymentPricing { .. }
            | OrchestratorCommand::RemoveDeploymentPricing { .. }
            | OrchestratorCommand::GitHubRefresh { .. }
            | OrchestratorCommand::GitHubOpen { .. }
            | OrchestratorCommand::GitHubCheckout { .. }
            | OrchestratorCommand::GitHubCreateDraft { .. }
            | OrchestratorCommand::SetPlanMode { .. }
            | OrchestratorCommand::SetExploreMode { .. }
            | OrchestratorCommand::SetReviewMode { .. }
            | OrchestratorCommand::SetDeepThinkingMode { .. }
            | OrchestratorCommand::SetAutoApprovalPolicy { .. }
            | OrchestratorCommand::SetGoal { .. }
            | OrchestratorCommand::ReloadProjectInstructions { .. }
            | OrchestratorCommand::SetProjectInstructionsEnabled { .. }
            | OrchestratorCommand::SetInstructionSourceEnabled { .. }
            | OrchestratorCommand::ReloadSkills { .. }
            | OrchestratorCommand::SetSkillEnabled { .. }
            | OrchestratorCommand::ReloadAutomation { .. }
            | OrchestratorCommand::SetHookEnabled { .. }
            | OrchestratorCommand::RefreshPlugins { .. }
            | OrchestratorCommand::AddPluginMarketplace { .. }
            | OrchestratorCommand::RemovePluginMarketplace { .. }
            | OrchestratorCommand::InstallLocalPlugin { .. }
            | OrchestratorCommand::InstallMarketplacePlugin { .. }
            | OrchestratorCommand::UpdatePlugin { .. }
            | OrchestratorCommand::SetPluginEnabled { .. }
            | OrchestratorCommand::RemovePlugin { .. }
            | OrchestratorCommand::McpConnect { .. }
            | OrchestratorCommand::McpDisconnect { .. }
            | OrchestratorCommand::McpSetEnabled { .. }
            | OrchestratorCommand::McpAddServer { .. }
            | OrchestratorCommand::SetSubagentMcpAccess { .. }
            | OrchestratorCommand::McpBeginOAuth { .. }
            | OrchestratorCommand::McpPollOAuth { .. }
            | OrchestratorCommand::McpForgetOAuth { .. }
            | OrchestratorCommand::LspConnect { .. }
            | OrchestratorCommand::LspDisconnect { .. }
            | OrchestratorCommand::LspSetEnabled { .. }
            | OrchestratorCommand::LspAddServer { .. }
            | OrchestratorCommand::LspRefresh { .. }
            | OrchestratorCommand::CodeIndexRefresh { .. }
            | OrchestratorCommand::CodeIndexCancel { .. }
            | OrchestratorCommand::CodeIndexPoll { .. }
            | OrchestratorCommand::CodeIndexSearch { .. }
            | OrchestratorCommand::ReloadPrivacy { .. }
            | OrchestratorCommand::RevokeSessionShellGrant { .. }
            | OrchestratorCommand::ClearSessionShellGrants { .. }
            | OrchestratorCommand::AskSideQuestion { .. }
            | OrchestratorCommand::CancelSideQuestion { .. }
            | OrchestratorCommand::EnqueueFollowUp { .. }
            | OrchestratorCommand::EditFollowUp { .. }
            | OrchestratorCommand::CancelFollowUp { .. }
            | OrchestratorCommand::RetryFollowUp { .. }
            | OrchestratorCommand::DispatchFollowUpQueue { .. }
            | OrchestratorCommand::DecideReviewFinding { .. }
            | OrchestratorCommand::SpawnSubagent { .. }
            | OrchestratorCommand::ReloadSubagentProfiles
            | OrchestratorCommand::MessageSubagent { .. }
            | OrchestratorCommand::CancelSubagent { .. }
            | OrchestratorCommand::ResumeSubagent { .. }
            | OrchestratorCommand::AbandonSubagentRecovery { .. }
            | OrchestratorCommand::DecideSubagentCommand { .. }
            | OrchestratorCommand::DecideSubagentBudget { .. }
            | OrchestratorCommand::OpenSubagentReview { .. }
            | OrchestratorCommand::DecideSubagentFile { .. }
            | OrchestratorCommand::Whip { .. }
            | OrchestratorCommand::Interrupt { .. }
            | OrchestratorCommand::ContinueToolLoop { .. }
            | OrchestratorCommand::RetryTurn { .. }
            | OrchestratorCommand::AbortTurn { .. } => AwaitedControl::Continue,
        }
    }

    async fn drain_busy_controls(&mut self, turn_id: TurnId) -> AwaitedControl {
        let urgent = self.drain_urgent_busy_controls(turn_id);
        if urgent != AwaitedControl::Continue {
            return urgent;
        }
        loop {
            match self.command_rx.try_recv() {
                Ok(command) => {
                    let result = self.handle_busy_control(command, turn_id).await;
                    if result != AwaitedControl::Continue {
                        return result;
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    return AwaitedControl::Continue;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    return AwaitedControl::Shutdown;
                }
            }
        }
    }

    async fn record_stopped_batch(&mut self, turn_id: TurnId, parsed: &[ParserEvent]) -> bool {
        for event in parsed {
            match event {
                ParserEvent::ToolCallParsed(action) => {
                    let action_id = self.allocate_action_id();
                    let outcome = ToolOutcome::Declined {
                        action: action.clone(),
                    };
                    self.state
                        .push_tool_result_with_action(turn_id, action_id, action, &outcome);
                    if !self
                        .emit(OrchestratorEvent::ToolCompleted {
                            conversation_epoch: self.conversation_epoch,
                            turn_id,
                            action_id,
                            action: action.clone(),
                            outcome,
                        })
                        .await
                    {
                        return false;
                    }
                }
                ParserEvent::ToolCallParseError { raw_tag, reason } => {
                    let action_id = self.allocate_action_id();
                    self.state.push_tool_diagnostic(
                        turn_id,
                        action_id,
                        "parser_error",
                        ToolResultStatus::ParseError,
                        format!(
                            "Tool batch was stopped at the iteration guard. Parse error: {reason}\nRaw tag: {raw_tag}"
                        ),
                    );
                }
                ParserEvent::ThinkingDelta(_)
                | ParserEvent::ThinkingEnd
                | ParserEvent::TurnComplete { .. } => {}
            }
        }
        self.emit_history().await
    }

    #[tracing::instrument(
        name = "checkpoint.begin_action",
        level = "debug",
        skip_all,
        fields(tool = action.tool_name())
    )]
    async fn begin_action_checkpoint(
        &mut self,
        action: &ToolAction,
    ) -> Result<Option<String>, TurnExit> {
        if !matches!(
            action,
            ToolAction::ApplyPatch { .. }
                | ToolAction::WriteFile { .. }
                | ToolAction::ExecuteCommand { .. }
        ) {
            return Ok(None);
        }
        let (Some(store), Some(_)) = (&self.checkpoint_store, &self.pending_checkpoint) else {
            return Ok(None);
        };
        store.begin_tool_segment().await.map(Some).map_err(|error| {
            TurnExit::Failed(format!(
                "tool was not started because its pre-action Git checkpoint failed: {error}"
            ))
        })
    }

    #[tracing::instrument(
        name = "checkpoint.finish_action",
        level = "debug",
        skip_all,
        fields(turn_id, action_id, tool = action.tool_name())
    )]
    async fn finish_checkpointed_action(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        outcome: ToolOutcome,
        finalization: ActionFinalization<'_>,
    ) -> Result<(), TurnExit> {
        let ActionFinalization {
            checkpoint_before,
            patch_approval,
            hook_cancel,
        } = finalization;
        let tracking_error = if let Some(before_tree) = checkpoint_before {
            let result = match (&self.checkpoint_store, &mut self.pending_checkpoint) {
                (Some(store), Some(pending)) => {
                    store.finish_tool_segment(pending, before_tree).await
                }
                _ => Ok(()),
            };
            result.err().map(|error| error.to_string())
        } else {
            None
        };
        if let Some(reason) = tracking_error.as_ref()
            && let Some(pending) = &mut self.pending_checkpoint
        {
            CheckpointStore::invalidate(pending, reason.clone());
        }

        let outcome = annotate_patch_outcome(outcome, patch_approval);
        self.finish_action(turn_id, action_id, action, outcome, hook_cancel)
            .await?;
        if let Some(reason) = tracking_error {
            return Err(TurnExit::Failed(format!(
                "tool completed, but its post-action Git checkpoint failed; rewind was disabled for this turn: {reason}"
            )));
        }
        Ok(())
    }

    async fn finish_action(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        outcome: ToolOutcome,
        hook_cancel: &CancellationToken,
    ) -> Result<(), TurnExit> {
        let post_hooks = self
            .run_hook_event(
                HookEvent::PostToolUse,
                Some(action.tool_name()),
                serde_json::json!({
                    "event": "post_tool_use",
                    "turn_id": turn_id,
                    "action_id": action_id,
                    "tool": action.tool_name(),
                    "action": &action,
                    "outcome": &outcome,
                }),
                hook_cancel,
            )
            .await;
        self.publish_hook_notes(&post_hooks);
        self.state
            .push_tool_result_with_action(turn_id, action_id, &action, &outcome);
        self.publish_privacy_snapshot();
        if !self
            .emit(OrchestratorEvent::ToolCompleted {
                conversation_epoch: self.conversation_epoch,
                turn_id,
                action_id,
                action,
                outcome,
            })
            .await
            || !self.emit_history().await
        {
            return Err(TurnExit::Shutdown);
        }
        Ok(())
    }

    async fn finish_action_without_hooks(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        action: ToolAction,
        outcome: ToolOutcome,
    ) -> Result<(), TurnExit> {
        self.state
            .push_tool_result_with_action(turn_id, action_id, &action, &outcome);
        self.publish_privacy_snapshot();
        if !self
            .emit(OrchestratorEvent::ToolCompleted {
                conversation_epoch: self.conversation_epoch,
                turn_id,
                action_id,
                action,
                outcome,
            })
            .await
            || !self.emit_history().await
        {
            return Err(TurnExit::Shutdown);
        }
        Ok(())
    }

    fn usage_snapshot(&self) -> UsageSnapshot {
        self.pricing.snapshot(
            &self.state.billing_usage,
            self.state.last_reported_total_tokens,
        )
    }

    async fn record_uncommitted_response_usage(
        &mut self,
        turn_id: TurnId,
        response: &ResponsesResponse,
    ) -> bool {
        let Some(usage) = &response.usage else {
            return true;
        };
        let represented_through = self.state.history.last().map_or(0, |entry| entry.sequence);
        self.state.record_deployment_usage(
            &self.deployment,
            usage.input_tokens,
            usage.cached_input_tokens(),
            usage.output_tokens,
            usage.total_tokens,
            represented_through,
        );
        self.emit(OrchestratorEvent::Usage {
            turn_id,
            usage: self.usage_snapshot(),
        })
        .await
    }

    fn publish_privacy_snapshot(&self) {
        match self.privacy.snapshot() {
            Ok(privacy) => self
                .snapshot_tx
                .send_modify(|snapshot| snapshot.privacy = privacy),
            Err(error) => tracing::error!(%error, "Privacy Shield snapshot is unavailable"),
        }
    }

    fn build_request(
        &self,
        turn_id: TurnId,
        penalty_applied: bool,
    ) -> Result<ResponsesRequest, super::state::ContextBudgetExceeded> {
        let context_budget = self.main_request_context_budget(turn_id);
        self.build_request_with_context_budget(turn_id, penalty_applied, context_budget)
    }

    fn build_request_with_context_budget(
        &self,
        turn_id: TurnId,
        penalty_applied: bool,
        context_budget: u32,
    ) -> Result<ResponsesRequest, super::state::ContextBudgetExceeded> {
        self.build_request_with_context_budget_and_pressure(
            turn_id,
            penalty_applied,
            context_budget,
            true,
        )
    }

    fn build_request_for_exact_preflight(
        &self,
        turn_id: TurnId,
        penalty_applied: bool,
        context_budget: u32,
    ) -> Result<ResponsesRequest, super::state::ContextBudgetExceeded> {
        self.build_request_with_context_budget_and_pressure(
            turn_id,
            penalty_applied,
            context_budget,
            false,
        )
    }

    fn build_request_with_context_budget_and_pressure(
        &self,
        turn_id: TurnId,
        penalty_applied: bool,
        context_budget: u32,
        use_reported_usage: bool,
    ) -> Result<ResponsesRequest, super::state::ContextBudgetExceeded> {
        let max_output_tokens = if penalty_applied {
            penalized_output_tokens(self.base_max_output_tokens, &self.agent_config.whip)
        } else {
            self.base_max_output_tokens
        };
        let configured_effort = if penalty_applied && !self.state.work_modes.any_enabled() {
            ReasoningEffort::Low
        } else {
            self.base_reasoning_effort
        };
        let (reasoning_effort, reasoning_mode) =
            self.state.work_modes.effective_reasoning(configured_effort);
        let instructions = self.effective_instructions();

        let pause_resume_note = (self.pause_resume_note_pending == Some(turn_id))
            .then(|| self.pause_resume_note(turn_id))
            .flatten();
        let transient_note_tokens = if self.whip_retry_note_pending == Some(turn_id) {
            approximate_tokens(WHIP_RETRY_NOTE).saturating_add(16)
        } else {
            0
        }
        .saturating_add(
            pause_resume_note
                .as_deref()
                .map(approximate_tokens)
                .unwrap_or_default()
                .saturating_add(u32::from(pause_resume_note.is_some()) * 16),
        );
        let context_budget = context_budget.min(
            self.request_context_budget()
                .saturating_sub(transient_note_tokens),
        );
        let mut request = match self.agent_config.context_mode {
            ContextMode::Stateless => {
                let input = if use_reported_usage {
                    self.state.checked_stateless_replay_input(context_budget)?
                } else {
                    self.state
                        .checked_stateless_replay_input_for_exact_preflight(context_budget)?
                };
                ResponsesRequest::stateless_replay(
                    &self.deployment,
                    &instructions,
                    input,
                    max_output_tokens,
                )
            }
            ContextMode::Stateful => {
                if let Some(previous_response_id) = &self.state.last_response_id {
                    ResponsesRequest::stateful(
                        &self.deployment,
                        &instructions,
                        history_to_input(self.state.checked_request_context_after(
                            self.state.represented_through,
                            context_budget,
                        )?),
                        max_output_tokens,
                        previous_response_id,
                    )
                } else {
                    let mut initial = ResponsesRequest::stateless(
                        &self.deployment,
                        &instructions,
                        history_to_input(
                            self.state
                                .checked_request_context_after(0, context_budget)?,
                        ),
                        max_output_tokens,
                    );
                    initial.store = true;
                    initial
                }
            }
        };
        if self.whip_retry_note_pending == Some(turn_id) {
            request.input.push(InputMessage::developer(WHIP_RETRY_NOTE));
        }
        if let Some(note) = pause_resume_note {
            request.input.push(InputMessage::developer(note));
        }
        request = request
            .with_reasoning_mode(reasoning_effort, reasoning_mode)
            .with_temperature(self.temperature);
        if let Some(context_management) = &self.context_management {
            request = request.with_context_management(context_management.clone());
        }
        let read_only = self.state.work_modes.read_only();
        let mut native_tools = if self.agent_config.subagents.enabled && !read_only {
            let profiles = self.subagents.snapshot().profiles;
            let profile_ids = profiles
                .profiles
                .iter()
                .map(|profile| profile.id.clone())
                .collect::<Vec<_>>();
            subagent_function_definitions(&profile_ids)
        } else {
            Vec::new()
        };
        if !read_only && let Some(mcp) = &self.mcp {
            native_tools.extend(
                mcp.tools()
                    .iter()
                    .map(crate::mcp::McpTool::function_definition),
            );
        }
        if let Some(lsp) = &self.lsp {
            native_tools.extend(lsp.function_definitions());
        }
        if let Some(code_index) = &self.code_index {
            native_tools.extend(code_index.function_definitions());
        }
        if self.skills.has_enabled_skills() {
            native_tools.extend(skill_function_definitions());
        }
        if self.state.work_modes.goal_enabled() {
            native_tools.push(goal_update_function_definition());
        }
        if self.state.work_modes.review {
            native_tools.extend(review_function_definitions());
        }
        request = request.with_tools(native_tools);
        Ok(request)
    }

    fn main_request_context_budget(&self, turn_id: TurnId) -> u32 {
        let pause_resume_tokens = (self.pause_resume_note_pending == Some(turn_id))
            .then(|| self.pause_resume_note(turn_id))
            .flatten()
            .as_deref()
            .map(approximate_tokens)
            .unwrap_or_default();
        let transient_tokens = if self.whip_retry_note_pending == Some(turn_id) {
            approximate_tokens(WHIP_RETRY_NOTE).saturating_add(16)
        } else {
            0
        }
        .saturating_add(
            pause_resume_tokens.saturating_add(u32::from(pause_resume_tokens > 0) * 16),
        );
        self.request_context_budget()
            .saturating_sub(transient_tokens)
    }

    fn should_preflight_exact_tokens(&self, request: &ResponsesRequest) -> bool {
        if !matches!(self.provider, ApiProvider::Azure | ApiProvider::OpenAi) {
            return false;
        }
        if request.has_attachment_input() {
            return true;
        }
        let half_limit = u64::from(self.agent_config.context_budget) / 2;
        let encoded_tokens = serde_json::to_vec(request)
            .map(|bytes| u64::try_from(bytes.len().saturating_add(3) / 4).unwrap_or(u64::MAX))
            .unwrap_or(u64::MAX);
        encoded_tokens.saturating_add(u64::from(request.max_output_tokens)) >= half_limit
            || self
                .state
                .last_reported_total_tokens
                .is_some_and(|tokens| tokens >= half_limit)
    }

    async fn fit_exact_request<F>(
        &mut self,
        mut request: ResponsesRequest,
        fallback_request: Option<ResponsesRequest>,
        maximum_replay_budget: u32,
        cancel: &CancellationToken,
        rebuild: F,
    ) -> Result<ResponsesRequest, String>
    where
        F: Fn(&Self, u32) -> Result<ResponsesRequest, super::state::ContextBudgetExceeded>,
    {
        self.attachment_store
            .hydrate_request(&mut request, self.client.capabilities())
            .map_err(|error| format!("attachment request was rejected: {error}"))?;
        let Some(full_input_tokens) = self
            .client
            .count_input_tokens(&request, cancel)
            .await
            .map_err(|error| format!("attachment token preflight failed: {error}"))?
        else {
            tracing::warn!(
                provider = ?self.provider,
                "provider has no exact input token counter; using conservative local budgeting"
            );
            return fallback_request.ok_or_else(|| {
                "request was not sent: the provider has no exact input token counter and conservative compaction cannot preserve the newest causal group"
                    .to_owned()
            });
        };
        let input_limit = u64::from(
            self.agent_config
                .context_budget
                .saturating_sub(request.max_output_tokens),
        );
        if full_input_tokens <= input_limit {
            return Ok(request);
        }

        let mut lower = 0_u32;
        let mut upper = maximum_replay_budget;
        while lower < upper {
            let middle = lower + (upper - lower) / 2;
            if rebuild(self, middle).is_ok() {
                upper = middle;
            } else {
                lower = middle.saturating_add(1);
            }
        }
        let minimum_budget = lower;
        let mut minimum = rebuild(self, minimum_budget).map_err(|error| {
            format!(
                "request was not sent because the first prompt anchor and newest causal group cannot fit safely: {error}"
            )
        })?;
        self.attachment_store
            .hydrate_request(&mut minimum, self.client.capabilities())
            .map_err(|error| format!("attachment request was rejected: {error}"))?;
        let minimum_input_tokens = if minimum.input == request.input {
            full_input_tokens
        } else {
            self.client
                .count_input_tokens(&minimum, cancel)
                .await
                .map_err(|error| format!("attachment token preflight failed: {error}"))?
                .ok_or_else(|| {
                    "provider token counting became unavailable during context compaction"
                        .to_owned()
                })?
        };
        if minimum_input_tokens > input_limit {
            let required =
                minimum_input_tokens.saturating_add(u64::from(request.max_output_tokens));
            return Err(format!(
                "request was not sent: the first prompt anchor and current prompt/attachments require {minimum_input_tokens} input tokens ({required} including reserved output), above the selected {}-token context limit. Older history is already reduced as far as safely possible; current attachments are never truncated. Split the files or choose a larger context limit",
                self.agent_config.context_budget
            ));
        }

        let mut best_budget = minimum_budget;
        let mut best_request = minimum;
        let mut fitting = minimum_budget;
        let mut overflowing = maximum_replay_budget;
        for _ in 0..MAX_EXACT_COMPACTION_PROBES {
            if overflowing <= fitting.saturating_add(1) {
                break;
            }
            let probe_budget = fitting + (overflowing - fitting) / 2;
            let mut candidate = rebuild(self, probe_budget).map_err(|error| error.to_string())?;
            self.attachment_store
                .hydrate_request(&mut candidate, self.client.capabilities())
                .map_err(|error| format!("attachment request was rejected: {error}"))?;
            if candidate.input == best_request.input {
                fitting = probe_budget;
                best_budget = probe_budget;
                continue;
            }
            if candidate.input == request.input {
                overflowing = probe_budget;
                continue;
            }
            let candidate_tokens = self
                .client
                .count_input_tokens(&candidate, cancel)
                .await
                .map_err(|error| format!("attachment token preflight failed: {error}"))?
                .ok_or_else(|| {
                    "provider token counting became unavailable during context compaction"
                        .to_owned()
                })?;
            if candidate_tokens <= input_limit {
                fitting = probe_budget;
                best_budget = probe_budget;
                best_request = candidate;
            } else {
                overflowing = probe_budget;
            }
        }

        self.state
            .compact_persisted_history_for_exact_preflight(best_budget)
            .map_err(|error| format!("exact context compaction failed: {error}"))?;
        if !self.emit_history_durable().await {
            return Err("compacted context could not be persisted".to_owned());
        }
        Ok(best_request)
    }

    fn pause_resume_note(&self, turn_id: TurnId) -> Option<String> {
        let draft = self.state.history.iter().rev().find(|entry| {
            entry.turn_id == turn_id
                && matches!(entry.kind, HistoryKind::Assistant)
                && matches!(entry.status, super::state::HistoryStatus::Interrupted)
        })?;
        let mut visible = String::new();
        for item in TagScanner::new(&draft.content) {
            if let ScanItem::UnexpectedText { text, .. } = item {
                for character in text.chars() {
                    if !character.is_control() || matches!(character, '\n' | '\r' | '\t') {
                        visible.push(character);
                    }
                }
            }
        }
        let visible = visible.trim();
        if visible.is_empty() {
            return Some(format!(
                "{PAUSE_RESUME_NOTE_PREFIX}\"No safe visible prose was completed before pause.\""
            ));
        }
        let excerpt = utf8_tail(visible, PAUSE_RESUME_EXCERPT_MAX_BYTES);
        let encoded = serde_json::to_string(excerpt).ok()?;
        Some(format!("{PAUSE_RESUME_NOTE_PREFIX}{encoded}"))
    }

    #[cfg(test)]
    fn build_plan_request(
        &self,
    ) -> Result<
        (
            ResponsesRequest,
            ReasoningEffort,
            Option<crate::api::ReasoningMode>,
        ),
        super::state::ContextBudgetExceeded,
    > {
        let mut instructions = self.effective_instructions();
        instructions.push_str("\n\n");
        instructions.push_str(PLAN_INSTRUCTIONS);
        let plan_budget = self.request_context_budget_for(&instructions);
        self.build_plan_request_with_context_budget(instructions, plan_budget)
    }

    fn build_plan_request_with_context_budget(
        &self,
        instructions: String,
        context_budget: u32,
    ) -> Result<
        (
            ResponsesRequest,
            ReasoningEffort,
            Option<crate::api::ReasoningMode>,
        ),
        super::state::ContextBudgetExceeded,
    > {
        self.build_plan_request_with_context_budget_and_pressure(instructions, context_budget, true)
    }

    fn build_plan_request_for_exact_preflight(
        &self,
        instructions: String,
        context_budget: u32,
    ) -> Result<
        (
            ResponsesRequest,
            ReasoningEffort,
            Option<crate::api::ReasoningMode>,
        ),
        super::state::ContextBudgetExceeded,
    > {
        self.build_plan_request_with_context_budget_and_pressure(
            instructions,
            context_budget,
            false,
        )
    }

    fn build_plan_request_with_context_budget_and_pressure(
        &self,
        instructions: String,
        context_budget: u32,
        use_reported_usage: bool,
    ) -> Result<
        (
            ResponsesRequest,
            ReasoningEffort,
            Option<crate::api::ReasoningMode>,
        ),
        super::state::ContextBudgetExceeded,
    > {
        let (reasoning_effort, reasoning_mode) = self
            .state
            .work_modes
            .effective_reasoning(self.base_reasoning_effort.at_least(ReasoningEffort::XHigh));
        let input = if use_reported_usage {
            self.state.checked_stateless_replay_input(context_budget)?
        } else {
            self.state
                .checked_stateless_replay_input_for_exact_preflight(context_budget)?
        };
        let request = ResponsesRequest::stateless_replay(
            &self.deployment,
            instructions,
            input,
            self.base_max_output_tokens,
        )
        .with_reasoning_mode(reasoning_effort, reasoning_mode)
        .with_temperature(self.temperature);
        Ok((request, reasoning_effort, reasoning_mode))
    }

    fn request_context_budget(&self) -> u32 {
        self.request_context_budget_for(&self.effective_instructions())
    }

    fn request_context_budget_for(&self, instructions: &str) -> u32 {
        let instruction_bytes = u32::try_from(instructions.len()).unwrap_or(u32::MAX);
        let instruction_tokens = instruction_bytes.saturating_add(3) / 4;
        self.agent_config
            .context_budget
            .saturating_sub(instruction_tokens)
            .saturating_sub(self.base_max_output_tokens)
    }

    fn effective_instructions(&self) -> String {
        let suffix = self.state.work_modes.instruction_suffix();
        let model_profile = gpt_coding_profile(self.provider, &self.deployment);
        let project = self.instructions.effective_fragment();
        let skills = self.skills.metadata_fragment();
        let index_guidance = if self
            .code_index
            .as_ref()
            .is_some_and(CodeIndexManager::is_enabled)
        {
            INDEX_SEARCH_INSTRUCTIONS
        } else {
            ""
        };
        let mut instructions = String::with_capacity(
            self.agent_config
                .instructions
                .len()
                .saturating_add(model_profile.len())
                .saturating_add(project.len())
                .saturating_add(skills.len())
                .saturating_add(suffix.len())
                .saturating_add(index_guidance.len())
                .saturating_add(READ_BATCH_INSTRUCTIONS.len()),
        );
        instructions.push_str(&self.agent_config.instructions);
        instructions.push_str(model_profile);
        instructions.push_str(&project);
        instructions.push_str(skills);
        instructions.push_str(&suffix);
        instructions.push_str(index_guidance);
        instructions.push_str(READ_BATCH_INSTRUCTIONS);
        instructions
    }

    fn register_whip(&mut self, turn_id: TurnId, now: Instant) -> WhipKind {
        let hard = self.last_whip.is_some_and(|(previous_turn, previous)| {
            previous_turn == turn_id
                && now.saturating_duration_since(previous)
                    <= self.agent_config.whip.double_hit_window
        });
        self.last_whip = if hard { None } else { Some((turn_id, now)) };
        if hard { WhipKind::Hard } else { WhipKind::Soft }
    }

    fn allocate_turn_id(&mut self) -> TurnId {
        let turn_id = self.next_turn_id;
        self.next_turn_id = self.next_turn_id.saturating_add(1);
        turn_id
    }

    fn allocate_action_id(&mut self) -> ActionId {
        let action_id = self.next_action_id;
        self.next_action_id = self.next_action_id.saturating_add(1);
        action_id
    }

    fn allocate_continuation_id(&mut self) -> ContinuationId {
        let continuation_id = self.next_continuation_id;
        self.next_continuation_id = self.next_continuation_id.saturating_add(1);
        continuation_id
    }

    fn allocate_plan_review_id(&mut self) -> u64 {
        let review_id = self.next_plan_review_id;
        self.next_plan_review_id = self.next_plan_review_id.saturating_add(1);
        review_id
    }

    fn approval_binding(
        &self,
        turn_id: TurnId,
        action_id: ActionId,
        action: &ToolAction,
    ) -> ApprovalBinding {
        let command = match action {
            ToolAction::ExecuteCommand { command, .. } => command.as_bytes(),
            _ => action.tool_name().as_bytes(),
        };
        let digest = match action {
            ToolAction::ExecuteCommand { command, .. } => CommandDigest::for_command(command),
            _ => CommandDigest::new(Sha256::digest(command).into()),
        };
        let mut nonce = [0_u8; 16];
        nonce[..8].copy_from_slice(&fastrand::u64(..).to_le_bytes());
        nonce[8..].copy_from_slice(&fastrand::u64(..).to_le_bytes());
        ApprovalBinding {
            epoch: self.conversation_epoch,
            turn_id,
            action_id,
            nonce: ApprovalNonce::new(nonce),
            command_digest: digest,
        }
    }

    async fn record_interrupted(&mut self, turn_id: TurnId, partial: String) -> bool {
        if partial.is_empty() {
            return true;
        }
        self.state
            .push_interrupted_assistant(turn_id, partial.clone());
        self.emit(OrchestratorEvent::AssistantInterrupted {
            turn_id,
            content: partial,
        })
        .await
            && self.emit_history().await
    }

    async fn record_superseded(&mut self, turn_id: TurnId, partial: String) -> bool {
        if partial.is_empty() {
            return true;
        }
        self.state
            .push_superseded_assistant(turn_id, partial.clone());
        self.emit(OrchestratorEvent::AssistantInterrupted {
            turn_id,
            content: partial,
        })
        .await
            && self.emit_history().await
    }

    async fn record_failed_draft(&mut self, turn_id: TurnId, partial: String) -> bool {
        if partial.is_empty() {
            return true;
        }
        self.state.push_failed_assistant(turn_id, partial.clone());
        self.emit(OrchestratorEvent::AssistantInterrupted {
            turn_id,
            content: partial,
        })
        .await
            && self.emit_history().await
    }

    async fn reset_state(&mut self) {
        self.cancel_side_task();
        self.queue_dispatch_request = None;
        self.active_review = None;
        self.state.reset();
        self.reset_session_shell_permissions();
        self.conversation_epoch = self.conversation_epoch.saturating_add(1);
        self.last_whip = None;
        self.penalty_responses_remaining = 0;
        self.whip_retry_note_pending = None;
        self.pause_resume_note_pending = None;
        self.retryable_turn = None;
        let _ = self.persist_current_session(true).await;
        // Publish reset as one epoch-changing snapshot transition. Emitting an
        // empty history or Idle phase first would let the UI observe a reset
        // before ResetAcknowledged and then accept stale diagnostics from the
        // previous epoch.
        let _ = self
            .emit(OrchestratorEvent::ResetAcknowledged {
                conversation_epoch: self.conversation_epoch,
            })
            .await;
    }

    fn accepts_idle_session_scope(&self, scope: CommandScope) -> bool {
        let snapshot = self.snapshot_tx.borrow();
        scope.conversation_epoch == snapshot.conversation_epoch
            && scope.phase_revision == snapshot.phase_revision
            && matches!(snapshot.phase, AgentPhase::Idle)
            && self.retryable_turn.is_none()
    }

    fn accepts_session_navigation_scope(&self, scope: CommandScope) -> bool {
        let snapshot = self.snapshot_tx.borrow();
        scope.conversation_epoch == snapshot.conversation_epoch
            && matches!(snapshot.phase, AgentPhase::Idle | AgentPhase::Error { .. })
    }

    fn accepts_runtime_session_scope(&self, scope: CommandScope) -> bool {
        let snapshot = self.snapshot_tx.borrow();
        scope.conversation_epoch == snapshot.conversation_epoch
            && scope.phase_revision == snapshot.phase_revision
            && (matches!(snapshot.phase, AgentPhase::Idle) && self.retryable_turn.is_none()
                || matches!(
                    snapshot.phase,
                    AgentPhase::Error {
                        recoverable: true,
                        ..
                    }
                ) && self.retryable_turn.is_some())
    }

    async fn ensure_current_session(&mut self, title_seed: &str) -> bool {
        if self.current_session_id.is_some() {
            return true;
        }
        match self
            .session_store
            .create(title_seed.to_owned(), self.state.clone(), None)
            .await
        {
            Ok(document) => {
                self.last_session_persist_revision = document.state.history_revision;
                self.last_session_persist_at = Instant::now();
                self.current_session_id = Some(document.summary.id.clone());
                if let Some(store) = &mut self.checkpoint_store {
                    store.set_active_session(Some(document.summary.id.to_string()));
                }
                self.publish_sessions(String::new(), false).await
                    && self.publish_checkpoints().await
            }
            Err(error) => {
                tracing::error!(%error, "could not create persistent session");
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Prompt was not submitted because its session journal could not be created: {error}"),
                })
                .await
            }
        }
    }

    async fn persist_current_session(&mut self, force: bool) -> bool {
        let Some(session_id) = self.current_session_id.clone() else {
            return true;
        };
        let revision_delta = self
            .state
            .history_revision
            .saturating_sub(self.last_session_persist_revision);
        if !force
            && revision_delta < SESSION_AUTOSAVE_REVISION_INTERVAL
            && self.last_session_persist_at.elapsed() < SESSION_AUTOSAVE_INTERVAL
        {
            return true;
        }
        let persisted_revision = self.state.history_revision;
        match self
            .session_store
            .save(session_id, self.state.clone())
            .await
        {
            Ok(_) => {
                self.last_session_persist_revision = persisted_revision;
                self.last_session_persist_at = Instant::now();
                true
            }
            Err(error) => {
                tracing::error!(%error, "could not persist session snapshot");
                let turn_id = self.snapshot_tx.borrow().active_turn_id;
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id,
                    message: format!("Session snapshot could not be persisted: {error}"),
                })
                .await
            }
        }
    }

    async fn publish_sessions(&self, query: String, include_archived: bool) -> bool {
        match self.session_store.list(query, include_archived).await {
            Ok(sessions) => {
                self.emit(OrchestratorEvent::SessionsUpdated {
                    sessions: Arc::from(sessions),
                    current_session_id: self.current_session_id.clone(),
                })
                .await
            }
            Err(error) => {
                tracing::error!(%error, "could not enumerate persistent sessions");
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session list could not be loaded: {error}"),
                })
                .await
            }
        }
    }

    async fn reload_project_instructions(&mut self, user_visible: bool) {
        let mut refreshed = self.instructions.clone();
        match tokio::task::spawn_blocking(move || {
            refreshed.reload();
            refreshed
        })
        .await
        {
            Ok(refreshed) => {
                self.instructions = refreshed;
                self.publish_instruction_snapshot(
                    user_visible.then_some("Repository instructions reloaded"),
                );
            }
            Err(error) => {
                tracing::error!(%error, "project-instruction refresh task failed");
                if user_visible {
                    self.snapshot_tx.send_modify(|snapshot| {
                        snapshot.status =
                            format!("Repository instructions could not be reloaded: {error}");
                    });
                }
            }
        }
    }

    fn publish_instruction_snapshot(&self, status: Option<&str>) {
        let instructions = self.instructions.snapshot();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.instructions = instructions;
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    async fn reload_skills(&mut self, user_visible: bool) {
        let mut refreshed = self.skills.clone();
        match tokio::task::spawn_blocking(move || {
            refreshed.reload();
            refreshed
        })
        .await
        {
            Ok(refreshed) => {
                self.skills = refreshed;
                self.publish_skills_snapshot(user_visible.then_some("Skills reloaded"));
            }
            Err(error) => {
                tracing::error!(%error, "skill refresh task failed");
                if user_visible {
                    self.snapshot_tx.send_modify(|snapshot| {
                        snapshot.status = format!("Skills could not be reloaded: {error}");
                    });
                }
            }
        }
    }

    fn publish_skills_snapshot(&self, status: Option<&str>) {
        let skills = self.skills.snapshot();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.skills = skills;
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    fn publish_automation_snapshot(&self, status: Option<&str>) {
        let automation = self.automation.lock().map_or_else(
            |_| AutomationSnapshot::default(),
            |catalog| catalog.snapshot(),
        );
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.automation = automation;
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    fn publish_plugin_snapshot(&self, status: Option<&str>) {
        let plugins = self.plugins.snapshot();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.plugins = plugins;
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    async fn finish_plugin_action(
        &mut self,
        result: Result<String, crate::plugins::PluginError>,
        reload_components: bool,
    ) {
        match result {
            Ok(status) => {
                if reload_components {
                    self.reload_skills(false).await;
                    self.reload_automation().await;
                    if let Err(error) = self.subagents.reload_profiles().await {
                        tracing::error!(%error, "plugin profile refresh failed");
                    }
                }
                self.publish_plugin_snapshot(Some(&status));
            }
            Err(error) => {
                tracing::warn!(%error, "plugin operation failed");
                self.publish_plugin_snapshot(Some(&format!("Plugin operation failed: {error}")));
            }
        }
    }

    async fn reload_automation(&self) {
        let automation = Arc::clone(&self.automation);
        match tokio::task::spawn_blocking(move || {
            let mut catalog = automation
                .lock()
                .map_err(|_| "automation catalog lock was poisoned".to_owned())?;
            catalog.reload();
            Ok::<_, String>(catalog.snapshot())
        })
        .await
        {
            Ok(Ok(automation)) => self.snapshot_tx.send_modify(|snapshot| {
                let commands = automation.commands.len();
                let hooks = automation.hooks.len();
                let diagnostics = automation.diagnostics.len();
                snapshot.automation = automation;
                snapshot.status = format!(
                    "Reloaded {commands} custom command(s), {hooks} hook(s), {diagnostics} diagnostic(s)"
                );
            }),
            Ok(Err(error)) => self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status = format!("Automation could not be reloaded: {error}");
            }),
            Err(error) => self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status = format!("Automation reload task failed: {error}");
            }),
        }
    }

    fn set_hook_enabled(&self, id: &str, enabled: bool) {
        let result = self
            .automation
            .lock()
            .map_err(|_| "automation catalog lock was poisoned".to_owned())
            .and_then(|mut catalog| {
                catalog
                    .set_hook_enabled(id, enabled)
                    .map_err(|error| error.to_string())?;
                Ok(catalog.snapshot())
            });
        match result {
            Ok(automation) => self.snapshot_tx.send_modify(|snapshot| {
                snapshot.automation = automation;
                snapshot.status = format!(
                    "Lifecycle hook {id} {} for this run",
                    if enabled { "enabled" } else { "disabled" }
                );
            }),
            Err(error) => self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status = format!("Hook state was not changed: {error}");
            }),
        }
    }

    async fn run_hook_event(
        &self,
        event: HookEvent,
        tool_name: Option<&str>,
        payload: Value,
        cancel: &CancellationToken,
    ) -> HookRunReport {
        let hooks = match self.automation.lock() {
            Ok(catalog) => catalog.matching_hooks(event, tool_name),
            Err(_) if event == HookEvent::PreToolUse => {
                return HookRunReport {
                    disposition: HookDisposition::Deny {
                        hook_id: "harness".to_owned(),
                        message:
                            "automation catalog became unavailable; pre-tool policy failed closed"
                                .to_owned(),
                    },
                    notes: Arc::from([]),
                };
            }
            Err(_) => Vec::new(),
        };
        if hooks.is_empty() {
            return HookRunReport {
                disposition: HookDisposition::Continue,
                notes: Arc::from([]),
            };
        }
        run_hooks(
            hooks,
            event,
            &payload,
            &self.tool_runner.sandbox_root(),
            cancel,
        )
        .await
    }

    fn publish_hook_notes(&self, report: &HookRunReport) {
        for note in report.notes.iter() {
            tracing::info!(bytes = note.len(), "lifecycle hook produced output");
        }
        if let Some(note) = report.notes.last() {
            self.snapshot_tx.send_modify(|snapshot| {
                snapshot.status = format!("Hook: {note}");
            });
        }
    }

    async fn publish_mcp_servers(&self) -> bool {
        let servers = self
            .mcp
            .as_ref()
            .map_or_else(Vec::new, McpManager::snapshots);
        self.emit(OrchestratorEvent::McpServersUpdated(Arc::from(servers)))
            .await
    }

    async fn handle_mcp_connect(&mut self, server: &str) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp.connect(server).await,
            None => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: "MCP is disabled in the trusted global configuration.".to_owned(),
                    })
                    .await;
                return;
            }
        };
        if let Err(error) = result {
            let _ = self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("MCP connection failed: {error}"),
                })
                .await;
        }
        let _ = self.publish_mcp_servers().await;
    }

    async fn handle_mcp_set_enabled(&mut self, server: &str, enabled: bool) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp.set_enabled(server, enabled).await,
            None => Err(crate::mcp::McpError::RuntimeDisabled),
        };
        if let Err(error) = result {
            let _ = self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("MCP switch could not be changed: {error}"),
                })
                .await;
            let _ = self.publish_mcp_servers().await;
            return;
        }
        if enabled {
            self.handle_mcp_connect(server).await;
        } else {
            let _ = self.publish_mcp_servers().await;
        }
    }

    async fn handle_mcp_add_server(&mut self, server: crate::mcp::McpServerConfig) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp
                .validate_add(&server)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    self.managed_connections
                        .save_mcp(&server)
                        .map_err(|error| error.to_string())
                })
                .and_then(|_| mcp.add_server(server).map_err(|error| error.to_string())),
            None => Err("MCP is disabled in the trusted global configuration".to_owned()),
        };
        match result {
            Ok(()) => {
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.status =
                        "MCP server saved. Click Connect when you are ready to start it."
                            .to_owned();
                });
            }
            Err(error) => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("MCP server was not added: {error}"),
                    })
                    .await;
            }
        }
        let _ = self.publish_mcp_servers().await;
    }

    async fn handle_mcp_begin_oauth(&mut self, server: &str) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp.begin_oauth(server).await,
            None => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: "MCP is disabled in the trusted global configuration.".to_owned(),
                    })
                    .await;
                return;
            }
        };
        match result {
            Ok(prompt) => {
                let _ = self.emit(OrchestratorEvent::McpOAuthPrompted(prompt)).await;
            }
            Err(error) => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("MCP OAuth could not start: {error}"),
                    })
                    .await;
            }
        }
        let _ = self.publish_mcp_servers().await;
    }

    async fn handle_mcp_poll_oauth(&mut self, server: &str) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp.poll_oauth(server).await,
            None => return,
        };
        match result {
            Ok(true) => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("MCP OAuth completed for {server}."),
                    })
                    .await;
            }
            Ok(false) => {}
            Err(error) => {
                let _ = self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("MCP OAuth failed: {error}"),
                    })
                    .await;
            }
        }
        let _ = self.publish_mcp_servers().await;
    }

    async fn handle_mcp_forget_oauth(&mut self, server: &str) {
        let result = match &mut self.mcp {
            Some(mcp) => mcp.forget_oauth(server).await,
            None => return,
        };
        if let Err(error) = result {
            let _ = self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("MCP credentials could not be removed: {error}"),
                })
                .await;
        }
        let _ = self.publish_mcp_servers().await;
    }

    fn publish_lsp_snapshot(&self, status: Option<&str>) {
        let servers = self
            .lsp
            .as_ref()
            .map_or_else(Vec::new, LspManager::snapshots);
        let diagnostics = self
            .lsp
            .as_ref()
            .map_or_else(Vec::new, LspManager::diagnostics);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.lsp_servers = Arc::from(servers);
            snapshot.lsp_diagnostics = Arc::from(diagnostics);
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    fn publish_code_index_snapshot(&self, status: Option<&str>) {
        let code_index = self
            .code_index
            .as_ref()
            .map_or_else(|| CodeIndexSnapshot::new(false), CodeIndexManager::snapshot);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.code_index = code_index;
            if let Some(status) = status {
                snapshot.status = status.to_owned();
            }
        });
    }

    fn publish_shell_permissions(&self, status: &str) {
        let permissions = self.session_shell_permissions.snapshot();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.shell_permissions = permissions;
            snapshot.status = status.to_owned();
        });
    }

    fn reset_session_shell_permissions(&mut self) {
        self.session_shell_permissions = SessionShellPermissions::default();
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.shell_permissions = ShellPermissionSnapshot::default();
        });
    }

    async fn handle_lsp_connect(&mut self, server: &str) {
        let result = match &mut self.lsp {
            Some(lsp) => lsp.connect(server).await,
            None => Err(crate::lsp::LspError::RuntimeDisabled),
        };
        let status = match result {
            Ok(()) => format!("Language server {server} is ready"),
            Err(error) => format!("Language server could not start: {error}"),
        };
        self.publish_lsp_snapshot(Some(&status));
    }

    async fn handle_lsp_set_enabled(&mut self, server: &str, enabled: bool) {
        let result = match &mut self.lsp {
            Some(lsp) => lsp.set_enabled(server, enabled).await,
            None => Err(crate::lsp::LspError::RuntimeDisabled),
        };
        let status = match result {
            Ok(()) => format!(
                "Language server {server} {} for this run",
                if enabled { "enabled" } else { "disabled" }
            ),
            Err(error) => format!("LSP switch could not be changed: {error}"),
        };
        self.publish_lsp_snapshot(Some(&status));
    }

    fn handle_lsp_add_server(&mut self, server: crate::lsp::LspServerConfig) {
        let result = match &mut self.lsp {
            Some(lsp) => lsp
                .validate_add(&server)
                .map_err(|error| error.to_string())
                .and_then(|()| {
                    self.managed_connections
                        .save_lsp(&server)
                        .map_err(|error| error.to_string())
                })
                .and_then(|_| lsp.add_server(server).map_err(|error| error.to_string())),
            None => Err("LSP is disabled in the trusted global configuration".to_owned()),
        };
        let status = result.map_or_else(
            |error| format!("Language server was not added: {error}"),
            |()| "Language server saved. Click Start to launch it for this run.".to_owned(),
        );
        self.publish_lsp_snapshot(Some(&status));
    }

    async fn start_new_session(&mut self) -> bool {
        if !self.prepare_session_transition().await {
            return false;
        }
        self.cancel_side_task();
        self.queue_dispatch_request = None;
        self.state.reset();
        self.state.session_context_budget = Some(self.default_context_budget);
        self.agent_config.context_budget = self.default_context_budget;
        self.reset_session_shell_permissions();
        self.state.work_modes = WorkModes::default();
        self.state.auto_approval = AutoApprovalPolicy::default();
        self.subagents.set_auto_approve_shell(false);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.work_modes = WorkModes::default();
            snapshot.auto_approval = AutoApprovalPolicy::default();
        });
        self.conversation_epoch = self.state.conversation_epoch;
        self.current_session_id = None;
        self.last_session_persist_revision = 0;
        self.last_session_persist_at = Instant::now();
        self.last_whip = None;
        self.penalty_responses_remaining = 0;
        self.whip_retry_note_pending = None;
        self.pause_resume_note_pending = None;
        self.retryable_turn = None;
        self.pending_checkpoint = None;
        self.active_review = None;
        if let Some(store) = &mut self.checkpoint_store {
            store.set_active_session(None);
        }
        if !self
            .emit(OrchestratorEvent::ResetAcknowledged {
                conversation_epoch: self.conversation_epoch,
            })
            .await
        {
            return false;
        }
        self.emit(OrchestratorEvent::RuntimeSettingsUpdated {
            deployment: self.deployment.clone(),
            reasoning_effort: self.base_reasoning_effort,
            context_budget: self.default_context_budget,
        })
        .await
            && self.publish_sessions(String::new(), false).await
            && self.publish_checkpoints().await
    }

    async fn resume_session(&mut self, id: SessionId, allow_workspace_mismatch: bool) -> bool {
        let reloading_current = self.current_session_id.as_ref() == Some(&id);
        if reloading_current && !self.prepare_session_transition().await {
            return false;
        }
        match self.session_store.load(id, allow_workspace_mismatch).await {
            Ok(document) => {
                if !reloading_current && !self.prepare_session_transition().await {
                    return false;
                }
                self.activate_session(document).await
            }
            Err(error) => {
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session was not resumed: {error}"),
                })
                .await
            }
        }
    }

    async fn fork_session(&mut self, id: SessionId) -> bool {
        if !self.prepare_session_transition().await {
            return false;
        }
        match self.session_store.fork(id).await {
            Ok(document) => self.activate_session(document).await,
            Err(error) => {
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session was not forked: {error}"),
                })
                .await
            }
        }
    }

    async fn prepare_session_transition(&mut self) -> bool {
        if let Some(turn_id) = self.retryable_turn {
            if self.state.paused_turn_id != Some(turn_id) {
                self.state.mark_turn_paused(turn_id);
            }
            self.state.finish_turn(turn_id);
        }
        self.finalize_checkpoint().await && self.persist_current_session(true).await
    }

    async fn prepare_shutdown(&mut self, turn_id: TurnId) {
        self.retryable_turn = Some(turn_id);
        self.state.mark_turn_paused(turn_id);
        self.state.finish_turn(turn_id);
        let _ = self.finalize_checkpoint().await;
        let _ = self.emit_history_durable().await;
        let _ = self.persist_current_session(true).await;
    }

    async fn rename_session(&self, id: SessionId, title: String) -> bool {
        match self.session_store.rename(id, title).await {
            Ok(_) => self.publish_sessions(String::new(), false).await,
            Err(error) => {
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session was not renamed: {error}"),
                })
                .await
            }
        }
    }

    async fn set_session_pinned(&self, id: SessionId, pinned: bool) -> bool {
        match self.session_store.set_pinned(id, pinned).await {
            Ok(_) => self.publish_sessions(String::new(), false).await,
            Err(error) => {
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session pin state was not changed: {error}"),
                })
                .await
            }
        }
    }

    async fn set_session_archived(&self, id: SessionId, archived: bool) -> bool {
        match self.session_store.set_archived(id, archived).await {
            Ok(_) => self.publish_sessions(String::new(), false).await,
            Err(error) => {
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Session archive state was not changed: {error}"),
                })
                .await
            }
        }
    }

    async fn github_refresh(&mut self) -> bool {
        self.publish_github_busy("Refreshing GitHub pull requests");
        let Some(github) = &mut self.github else {
            return self.publish_github_error("GitHub integration is unavailable");
        };
        match github.refresh().await {
            Ok(snapshot) => {
                self.snapshot_tx.send_modify(|state| {
                    state.status.clone_from(&snapshot.status);
                    state.github = snapshot;
                });
                true
            }
            Err(error) => self.publish_github_error(&error.to_string()),
        }
    }

    async fn github_open(&mut self, number: u64) -> bool {
        self.publish_github_busy(&format!("Opening pull request #{number}"));
        let Some(github) = &self.github else {
            return self.publish_github_error("GitHub integration is unavailable");
        };
        match github.open(number).await {
            Ok(()) => {
                self.snapshot_tx.send_modify(|state| {
                    state.status = format!("Opened pull request #{number} in the browser");
                });
                true
            }
            Err(error) => self.publish_github_error(&error.to_string()),
        }
    }

    async fn github_checkout(&mut self, number: u64) -> bool {
        self.publish_github_busy(&format!("Checking out pull request #{number}"));
        let Some(github) = &mut self.github else {
            return self.publish_github_error("GitHub integration is unavailable");
        };
        match github.checkout(number).await {
            Ok(snapshot) => {
                self.snapshot_tx.send_modify(|state| {
                    state.status = format!("Checked out pull request #{number}");
                    state.github = snapshot;
                });
                true
            }
            Err(error) => self.publish_github_error(&error.to_string()),
        }
    }

    async fn github_create_draft(&mut self) -> bool {
        self.publish_github_busy("Creating a draft pull request");
        let Some(github) = &mut self.github else {
            return self.publish_github_error("GitHub integration is unavailable");
        };
        match github.create_draft_from_commits().await {
            Ok(snapshot) => {
                self.snapshot_tx.send_modify(|state| {
                    state.status = "Created a draft pull request from local commits".to_owned();
                    state.github = snapshot;
                });
                true
            }
            Err(error) => self.publish_github_error(&error.to_string()),
        }
    }

    fn publish_github_error(&self, message: &str) -> bool {
        self.snapshot_tx.send_modify(|state| {
            state.github.busy = false;
            state.github.status = format!("GitHub: {message}");
            state.github.revision = state.github.revision.saturating_add(1);
            state.status.clone_from(&state.github.status);
        });
        true
    }

    fn publish_github_busy(&self, status: &str) {
        self.snapshot_tx.send_modify(|state| {
            state.github.busy = true;
            state.github.status = status.to_owned();
            state.github.revision = state.github.revision.saturating_add(1);
            state.status = status.to_owned();
        });
    }

    async fn update_runtime_settings(
        &mut self,
        deployment: String,
        reasoning_effort: ReasoningEffort,
        deep_thinking: bool,
        context_budget: u32,
    ) -> bool {
        let deployment = deployment.trim();
        if deployment.is_empty()
            || deployment.len() > 256
            || deployment.chars().any(char::is_control)
        {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: "Deployment name must contain visible text and be at most 256 bytes."
                        .to_owned(),
                })
                .await;
        }
        if context_budget == 0 || context_budget > self.agent_config.max_context_budget {
            return self
                .emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!(
                        "Context budget must be between 1 and the configured model ceiling of {} tokens.",
                        self.agent_config.max_context_budget
                    ),
                })
                .await;
        }
        self.deployment = deployment.to_owned();
        self.base_reasoning_effort = reasoning_effort;
        self.agent_config.context_budget = context_budget;
        self.default_context_budget = context_budget;
        self.state.session_context_budget = Some(context_budget);
        self.state.work_modes.deep_thinking = deep_thinking;
        // A response chain belongs to the deployment that created it. The
        // next stateful request must rebuild context instead of attaching a
        // response ID minted by a different deployment.
        self.state.last_response_id = None;
        self.state.represented_through = 0;
        if !self.work_modes_changed().await {
            return false;
        }
        self.emit(OrchestratorEvent::RuntimeSettingsUpdated {
            deployment: self.deployment.clone(),
            reasoning_effort,
            context_budget,
        })
        .await
    }

    async fn update_work_modes(&mut self, update: impl FnOnce(&mut WorkModes)) -> bool {
        update(&mut self.state.work_modes);
        self.work_modes_changed().await
    }

    async fn work_modes_changed(&mut self) -> bool {
        self.state.last_response_id = None;
        self.state.represented_through = 0;
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.work_modes = self.state.work_modes.clone();
            let (effort, mode) = self
                .state
                .work_modes
                .effective_reasoning(self.base_reasoning_effort);
            snapshot.status = match mode {
                Some(mode) => format!("Work modes updated · effective reasoning {effort}/{mode}"),
                None => format!("Work modes updated · effective reasoning {effort}"),
            };
        });
        self.persist_current_session(true).await
    }

    async fn update_auto_approval(&mut self, policy: AutoApprovalPolicy) -> bool {
        self.state.auto_approval = policy;
        self.subagents.set_auto_approve_shell(policy.subagent_shell);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.auto_approval = policy;
            snapshot.status = format!(
                "Auto-Approval Center updated · {}/8 enabled · forced safety rules remain active",
                policy.enabled_count()
            );
        });
        self.persist_current_session(true).await
    }

    async fn decide_review_finding(
        &mut self,
        report_id: u64,
        revision: u64,
        finding_id: u64,
        decision: ReviewFindingDecision,
    ) -> bool {
        if decision == ReviewFindingDecision::QueueFix {
            let prompt = match self
                .state
                .reviews
                .fix_prompt(report_id, revision, finding_id)
            {
                Ok(prompt) => prompt,
                Err(error) => {
                    return self
                        .emit(OrchestratorEvent::RecoverableError {
                            turn_id: None,
                            message: format!("Review finding was not queued: {error}"),
                        })
                        .await;
                }
            };
            if let Err(error) = self
                .state
                .follow_ups
                .enqueue_manual_queue(prompt, UiNotice::FollowUpRecoveredPending)
            {
                return self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("Review finding was not queued: {error}"),
                    })
                    .await;
            }
        }
        match self
            .state
            .reviews
            .decide(report_id, revision, finding_id, decision)
        {
            Ok(()) => {
                let reviews = self.state.reviews.snapshot();
                let follow_ups = self.state.follow_ups.snapshot();
                self.snapshot_tx.send_modify(|snapshot| {
                    snapshot.reviews = reviews;
                    snapshot.follow_ups = follow_ups;
                    snapshot.status = match decision {
                        ReviewFindingDecision::Accept => {
                            "Review finding accepted for manual resolution".to_owned()
                        }
                        ReviewFindingDecision::Dismiss => "Review finding dismissed".to_owned(),
                        ReviewFindingDecision::QueueFix => {
                            "Review fix staged; inspect modes and click Run next".to_owned()
                        }
                    };
                });
                self.persist_current_session(true).await
            }
            Err(error) => {
                // The single-threaded actor prevents a revision race between
                // fix_prompt and decide. If this ever fires after enqueue,
                // leave the visible queued item intact instead of hiding work.
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!("Review finding decision was not applied: {error}"),
                })
                .await
            }
        }
    }

    async fn activate_session(&mut self, document: SessionDocument) -> bool {
        let previous_epoch = self
            .conversation_epoch
            .max(self.snapshot_tx.borrow().conversation_epoch);
        self.cancel_side_task();
        self.queue_dispatch_request = None;
        self.state = document.state;
        let context_budget = self
            .state
            .session_context_budget
            .filter(|budget| *budget > 0 && *budget <= self.agent_config.max_context_budget)
            .unwrap_or(self.default_context_budget);
        self.state.session_context_budget = Some(context_budget);
        self.agent_config.context_budget = context_budget;
        self.state.advance_conversation_epoch_past(previous_epoch);
        self.reset_session_shell_permissions();
        self.conversation_epoch = self.state.conversation_epoch;
        self.current_session_id = Some(document.summary.id.clone());
        self.next_turn_id = self
            .state
            .history
            .iter()
            .map(|entry| entry.turn_id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.next_action_id = self
            .state
            .history
            .iter()
            .filter_map(|entry| match entry.kind {
                HistoryKind::ToolResult { action_id, .. } => Some(action_id),
                HistoryKind::User | HistoryKind::Assistant => None,
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.next_continuation_id = 1;
        self.retryable_turn = self.state.paused_turn_id;
        self.pending_checkpoint = None;
        self.last_whip = None;
        self.whip_retry_note_pending = None;
        self.penalty_responses_remaining = 0;
        self.subagents
            .set_auto_approve_shell(self.state.auto_approval.subagent_shell);
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.work_modes = self.state.work_modes.clone();
            snapshot.auto_approval = self.state.auto_approval;
            snapshot.reviews = self.state.reviews.snapshot();
        });
        if let Some(store) = &mut self.checkpoint_store {
            store.set_active_session(Some(document.summary.id.to_string()));
        }
        if !self.persist_current_session(true).await {
            return false;
        }
        let history = self.bounded_ui_history();
        let usage = self.usage_snapshot();
        let side_chat = self.state.side_chat.snapshot();
        let follow_ups = self.state.follow_ups.snapshot();
        let paused_turn_id = self.state.paused_turn_id;
        self.emit(OrchestratorEvent::SessionActivated {
            conversation_epoch: self.conversation_epoch,
            summary: document.summary,
            history,
            usage,
            side_chat,
            follow_ups,
            paused_turn_id,
            context_budget,
        })
        .await
            && self.publish_sessions(String::new(), false).await
            && self.publish_checkpoints().await
            && self.set_phase(None, AgentPhase::Idle).await
    }

    #[tracing::instrument(
        name = "checkpoint.finalize",
        level = "debug",
        skip_all,
        fields(session_id = ?self.current_session_id)
    )]
    async fn finalize_checkpoint(&mut self) -> bool {
        let Some(pending) = self.pending_checkpoint.take() else {
            return true;
        };
        let Some(store) = &mut self.checkpoint_store else {
            return true;
        };
        match store.commit(pending).await {
            Ok(_) => self.publish_checkpoints().await,
            Err(error) => {
                tracing::error!(%error, "could not finalize Git checkpoint");
                self.emit(OrchestratorEvent::RecoverableError {
                    turn_id: None,
                    message: format!(
                        "The turn finished, but its Git checkpoint could not be retained: {error}"
                    ),
                })
                .await
            }
        }
    }

    async fn publish_checkpoints(&self) -> bool {
        let summaries = self
            .checkpoint_store
            .as_ref()
            .map_or_else(Vec::new, CheckpointStore::summaries);
        self.emit(OrchestratorEvent::CheckpointsUpdated(Arc::from(summaries)))
            .await
    }

    #[tracing::instrument(
        name = "checkpoint.rewind",
        level = "info",
        skip_all,
        fields(session_id = ?self.current_session_id, checkpoint_id)
    )]
    async fn rewind_checkpoint(&mut self, checkpoint_id: u64) -> bool {
        self.queue_dispatch_request = None;
        self.active_review = None;
        if let Some(request_id) = self
            .state
            .side_chat
            .snapshot()
            .latest()
            .filter(|exchange| exchange.status == SideExchangeStatus::Running)
            .map(|exchange| exchange.id)
        {
            self.cancel_side_task();
            let _ = self.state.side_chat.cancel(request_id);
        }
        if let Some(turn_id) = self.snapshot_tx.borrow().active_turn_id {
            self.state
                .follow_ups
                .fail_pending_steers_for_turn(turn_id, UiNotice::FollowUpInterrupted);
        }
        let result = match &mut self.checkpoint_store {
            Some(store) => store.rewind(checkpoint_id).await,
            None => {
                return self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: "Checkpoint/rewind is unavailable because this workspace is not a Git repository."
                            .to_owned(),
                    })
                    .await;
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                return self
                    .emit(OrchestratorEvent::RecoverableError {
                        turn_id: None,
                        message: format!("Rewind was not applied: {error}"),
                    })
                    .await;
            }
        };

        self.state.restore_checkpoint(&result.state_before);
        self.reset_session_shell_permissions();
        self.subagents
            .set_auto_approve_shell(self.state.auto_approval.subagent_shell);
        let usage = self.usage_snapshot();
        let paused_turn_id = self.state.paused_turn_id;
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.work_modes = self.state.work_modes.clone();
            snapshot.auto_approval = self.state.auto_approval;
            snapshot.side_chat = self.state.side_chat.snapshot();
            snapshot.follow_ups = self.state.follow_ups.snapshot();
            snapshot.reviews = self.state.reviews.snapshot();
            snapshot.usage = Some(usage);
            snapshot.paused_turn_id = paused_turn_id;
        });
        self.conversation_epoch = self.state.conversation_epoch;
        self.last_whip = None;
        self.penalty_responses_remaining = 0;
        self.whip_retry_note_pending = None;
        self.retryable_turn = None;
        if !self.persist_current_session(true).await {
            return false;
        }
        let history = self.bounded_ui_history();
        self.emit(OrchestratorEvent::RewindCompleted {
            conversation_epoch: self.conversation_epoch,
            report: result.report,
            history,
        })
        .await
            && self.publish_checkpoints().await
            && self.set_phase(None, AgentPhase::Idle).await
    }

    async fn emit_history(&mut self) -> bool {
        let context_budget = self.request_context_budget();
        let stateful = matches!(self.agent_config.context_mode, ContextMode::Stateful);
        if let Err(error) = self
            .state
            .compact_persisted_history(context_budget, stateful)
        {
            // The authoritative request builder will fail closed before any
            // API call. Keep the newest raw causal group available for the
            // recoverable error/Reset path instead of silently truncating it.
            tracing::warn!(%error, "persistent history could not preserve the newest causal group");
        }
        let history = self.bounded_ui_history();
        self.persist_current_session(false).await
            && self.emit(OrchestratorEvent::HistorySnapshot(history)).await
    }

    async fn emit_history_durable(&mut self) -> bool {
        let context_budget = self.request_context_budget();
        let stateful = matches!(self.agent_config.context_mode, ContextMode::Stateful);
        if let Err(error) = self
            .state
            .compact_persisted_history(context_budget, stateful)
        {
            tracing::warn!(%error, "persistent history could not preserve the newest causal group");
        }
        let history = self.bounded_ui_history();
        self.persist_current_session(true).await
            && self.emit(OrchestratorEvent::HistorySnapshot(history)).await
    }

    fn bounded_ui_history(&self) -> Arc<[HistoryEntry]> {
        let mut newest: Vec<(HistoryEntry, usize)> = Vec::new();
        let mut used_bytes = 0_usize;
        let content_budget = UI_HISTORY_MAX_BYTES.saturating_sub(UI_HISTORY_SUMMARY_RESERVE_BYTES);
        let visible_history = self.state.visible_history();

        for entry in visible_history.iter().rev() {
            if newest.len() >= UI_HISTORY_MAX_ENTRIES {
                break;
            }
            let mut visible = (*entry).clone();
            // Opaque response items are API context, never terminal content.
            visible.api_items.clear();
            truncate_back_utf8(&mut visible.content, UI_HISTORY_ENTRY_MAX_BYTES);
            let mut cost = ui_history_entry_bytes(&visible);
            if used_bytes.saturating_add(cost) > content_budget {
                if newest.is_empty() {
                    let metadata_bytes = cost.saturating_sub(visible.content.len());
                    truncate_back_utf8(
                        &mut visible.content,
                        content_budget.saturating_sub(metadata_bytes),
                    );
                    visible.approx_tokens = approximate_tokens(&visible.content);
                    cost = ui_history_entry_bytes(&visible);
                    if cost <= content_budget {
                        newest.push((visible, cost));
                    }
                }
                break;
            }
            used_bytes = used_bytes.saturating_add(cost);
            visible.approx_tokens = approximate_tokens(&visible.content);
            newest.push((visible, cost));
        }

        newest.reverse();
        let mut omitted = visible_history.len().saturating_sub(newest.len());
        if omitted > 0 {
            if newest.len() >= UI_HISTORY_MAX_ENTRIES && !newest.is_empty() {
                let (_, removed_cost) = newest.remove(0);
                used_bytes = used_bytes.saturating_sub(removed_cost);
                omitted = omitted.saturating_add(1);
            }
            let sequence = newest
                .first()
                .map_or(0, |(entry, _)| entry.sequence.saturating_sub(1));
            let content = format!(
                "[{omitted} older history entries summarized to keep the UI snapshot bounded]"
            );
            let summary = HistoryEntry {
                epoch: self.conversation_epoch,
                revision: self.state.history_revision,
                sequence,
                turn_id: 0,
                kind: HistoryKind::Assistant,
                content: content.clone(),
                attachments: Vec::new(),
                status: super::state::HistoryStatus::Committed,
                approx_tokens: approximate_tokens(&content),
                created_at: chrono::Utc::now(),
                api_items: Vec::new(),
                tool_summary: None,
                turn_metrics: None,
            };
            let summary_cost = ui_history_entry_bytes(&summary);
            debug_assert!(
                used_bytes.saturating_add(summary_cost) <= UI_HISTORY_MAX_BYTES,
                "reserved UI history summary space must be sufficient"
            );
            newest.insert(0, (summary, summary_cost));
        }
        Arc::from(
            newest
                .into_iter()
                .map(|(entry, _)| entry)
                .collect::<Vec<_>>(),
        )
    }

    async fn set_phase(&self, turn_id: Option<TurnId>, phase: AgentPhase) -> bool {
        self.emit(OrchestratorEvent::PhaseChanged { turn_id, phase })
            .await
    }

    async fn emit(&self, event: OrchestratorEvent) -> bool {
        self.update_snapshot(&event);
        let _diagnostic_delivery = self.event_tx.try_send(event);
        true
    }

    fn emit_transient(&self, event: OrchestratorEvent) -> bool {
        self.update_snapshot(&event);
        let _diagnostic_delivery = self.event_tx.try_send(event);
        true
    }

    fn update_snapshot(&self, event: &OrchestratorEvent) {
        self.snapshot_tx.send_modify(|snapshot| {
            snapshot.notice = UiNotice::None;
            match event {
            OrchestratorEvent::PhaseChanged { turn_id, phase } => {
                let starts_new_turn = turn_id.is_some() && *turn_id != snapshot.active_turn_id;
                snapshot.phase_revision = snapshot.phase_revision.saturating_add(1);
                snapshot.phase = phase.clone();
                snapshot.active_turn_id = *turn_id;
                if starts_new_turn {
                    snapshot.thinking.clear();
                    snapshot.assistant.clear();
                    snapshot.interrupted_draft.clear();
                    snapshot.retry = None;
                }
                if !matches!(
                    phase,
                    AgentPhase::AwaitingPlanApproval
                        | AgentPhase::AwaitingPatchApproval
                        | AgentPhase::AwaitingConfirmation
                        | AgentPhase::AwaitingContinuation
                ) {
                    snapshot.modal = None;
                }
                snapshot.connection_status =
                    match phase {
                        AgentPhase::PreparingReview => "capturing review snapshot",
                        AgentPhase::Planning => "planning (read-only)",
                        AgentPhase::AwaitingPlanApproval => "awaiting plan approval",
                        AgentPhase::Requesting => match self.client.transport() {
                            crate::config::ApiTransport::WebSocket => "WebSocket connecting",
                            crate::config::ApiTransport::Auto
                            | crate::config::ApiTransport::Sse => "SSE connecting",
                        },
                        AgentPhase::Streaming => match self.client.transport() {
                            crate::config::ApiTransport::WebSocket => "WebSocket streaming",
                            crate::config::ApiTransport::Auto
                            | crate::config::ApiTransport::Sse => "SSE streaming",
                        },
                        AgentPhase::Parsing => "parsing",
                        AgentPhase::AwaitingPatchApproval => "awaiting patch approval",
                        AgentPhase::AwaitingConfirmation => "awaiting confirmation",
                        AgentPhase::ExecutingTools => "executing tool",
                        AgentPhase::AwaitingContinuation => "awaiting continuation",
                        AgentPhase::Idle => "idle",
                        AgentPhase::Error { .. } => "error",
                    }
                    .to_owned();
            }
            OrchestratorEvent::ThinkingDelta { turn_id, delta } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.thinking.push_str(delta);
                truncate_front_utf8_amortized(&mut snapshot.thinking, 256 * 1024, 512 * 1024);
            }
            OrchestratorEvent::AssistantCommitted { turn_id, content } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.thinking.clear();
                snapshot.interrupted_draft.clear();
                snapshot.assistant.clone_from(content);
                snapshot.modal = None;
                snapshot.retry = None;
                snapshot.status = "Response completed".to_owned();
            }
            OrchestratorEvent::AssistantInterrupted { turn_id, content } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.thinking.clear();
                snapshot.assistant.clear();
                snapshot.interrupted_draft.clone_from(content);
                truncate_front_utf8(&mut snapshot.interrupted_draft, 256 * 1024);
                snapshot.status = "Response interrupted; draft was not committed".to_owned();
            }
            OrchestratorEvent::ToolStarted {
                turn_id,
                action_id,
                action,
                ..
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = None;
                remember_tool_action(&mut snapshot.tool_actions, *action_id, action);
                snapshot.status = format!("Tool #{action_id}: {} running", action.tool_name());
            }
            OrchestratorEvent::ToolCompleted {
                turn_id,
                action_id,
                action,
                ..
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = None;
                remember_tool_action(&mut snapshot.tool_actions, *action_id, action);
                snapshot.status = format!("Tool #{action_id}: {} completed", action.tool_name());
            }
            OrchestratorEvent::McpToolStarted {
                turn_id,
                action_id,
                call,
                ..
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = None;
                remember_mcp_call(&mut snapshot.mcp_calls, *action_id, call);
                snapshot.status =
                    format!("MCP #{action_id}: {}::{} running", call.server, call.tool);
            }
            OrchestratorEvent::McpToolCompleted {
                turn_id,
                action_id,
                call,
                outcome,
                ..
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = None;
                remember_mcp_call(&mut snapshot.mcp_calls, *action_id, call);
                let state = if outcome.is_error {
                    "failed"
                } else {
                    "completed"
                };
                snapshot.status =
                    format!("MCP #{action_id}: {}::{} {state}", call.server, call.tool);
            }
            OrchestratorEvent::ConfirmationRequested {
                turn_id,
                action_id,
                action,
                command,
                command_bytes,
                command_digest,
                model_requested,
                reason,
                session_trust_available,
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = Some(UiModal::Confirmation {
                    turn_id: *turn_id,
                    action_id: *action_id,
                    action: action.clone(),
                    command: command.clone(),
                    command_bytes: *command_bytes,
                    command_digest: *command_digest,
                    model_requested: *model_requested,
                    reason: *reason,
                    session_trust_available: *session_trust_available,
                });
                snapshot.status = "Command approval required".to_owned();
            }
            OrchestratorEvent::McpConfirmationRequested {
                turn_id,
                action_id,
                call,
                reason,
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = Some(UiModal::McpConfirmation {
                    turn_id: *turn_id,
                    action_id: *action_id,
                    call: Arc::clone(call),
                    reason: reason.clone(),
                });
                snapshot.status = format!("MCP approval required: {}::{}", call.server, call.tool);
            }
            OrchestratorEvent::PatchApprovalRequested {
                turn_id,
                action_id,
                review,
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = Some(UiModal::PatchApproval {
                    turn_id: *turn_id,
                    action_id: *action_id,
                    review: Arc::clone(review),
                });
                snapshot.status =
                    format!("Patch approval required: {} hunk(s)", review.hunks.len());
            }
            OrchestratorEvent::ContinuationRequested {
                turn_id,
                continuation_id,
                completed_iterations,
                max_iterations,
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.modal = Some(UiModal::Continuation {
                    turn_id: *turn_id,
                    continuation_id: *continuation_id,
                    completed_iterations: *completed_iterations,
                    max_iterations: *max_iterations,
                });
                snapshot.status = "Tool-iteration limit reached".to_owned();
            }
            OrchestratorEvent::WhipAcknowledged { turn_id, kind, .. } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.whip.total_strikes = snapshot.whip.total_strikes.saturating_add(1);
                snapshot.whip.penalty_responses_remaining = self.penalty_responses_remaining;
                snapshot.status = format!("Whip accepted: {kind:?}");
            }
            OrchestratorEvent::McpServersUpdated(servers) => {
                snapshot.mcp_servers = Arc::clone(servers);
                if let Some(prompt) = &snapshot.mcp_oauth_prompt
                    && servers.iter().any(|server| {
                        server.name == prompt.server
                            && !matches!(
                                server.state,
                                McpConnectionState::ReauthRequired | McpConnectionState::Connecting
                            )
                    })
                {
                    snapshot.mcp_oauth_prompt = None;
                }
            }
            OrchestratorEvent::McpOAuthPrompted(prompt) => {
                snapshot.mcp_oauth_prompt = Some(prompt.clone());
                snapshot.status = format!("Waiting for OAuth callback: {}", prompt.server);
            }
            OrchestratorEvent::ResetAcknowledged { conversation_epoch } => {
                let session_whip = WhipTelemetry {
                    total_strikes: snapshot.whip.total_strikes,
                    penalty_responses_remaining: 0,
                    estimated_saved_token_budget: snapshot.whip.estimated_saved_token_budget,
                };
                let sessions = Arc::clone(&snapshot.sessions);
                let checkpoints = Arc::clone(&snapshot.checkpoints);
                let current_session_id = self.current_session_id.clone();
                let deployment = snapshot.deployment.clone();
                let reasoning_effort = snapshot.reasoning_effort;
                let context_budget = snapshot.context_budget;
                let max_context_budget = snapshot.max_context_budget;
                let github = snapshot.github.clone();
                let instructions = snapshot.instructions.clone();
                let skills = snapshot.skills.clone();
                let automation = snapshot.automation.clone();
                let plugins = snapshot.plugins.clone();
                let mcp_servers = Arc::clone(&snapshot.mcp_servers);
                let mcp_oauth_prompt = snapshot.mcp_oauth_prompt.clone();
                let lsp_servers = Arc::clone(&snapshot.lsp_servers);
                let lsp_diagnostics = Arc::clone(&snapshot.lsp_diagnostics);
                let code_index = snapshot.code_index.clone();
                let code_index_hits = Arc::clone(&snapshot.code_index_hits);
                let privacy = snapshot.privacy.clone();
                let subagents = snapshot.subagents.clone();
                let work_modes = self.state.work_modes.clone();
                let auto_approval = self.state.auto_approval;
                let reviews = self.state.reviews.snapshot();
                *snapshot = UiSnapshot {
                    conversation_epoch: *conversation_epoch,
                    phase_revision: snapshot.phase_revision.saturating_add(1),
                    history_revision: snapshot.history_revision.saturating_add(1),
                    whip: session_whip,
                    sessions,
                    checkpoints,
                    current_session_id,
                    deployment,
                    reasoning_effort,
                    context_budget,
                    max_context_budget,
                    github,
                    instructions,
                    skills,
                    automation,
                    plugins,
                    mcp_servers,
                    mcp_oauth_prompt,
                    lsp_servers,
                    lsp_diagnostics,
                    code_index,
                    code_index_hits,
                    privacy,
                    subagents,
                    work_modes,
                    auto_approval,
                    reviews,
                    status: "Conversation reset".to_owned(),
                    ..UiSnapshot::default()
                };
            }
            OrchestratorEvent::CheckpointsUpdated(checkpoints) => {
                snapshot.checkpoints = Arc::clone(checkpoints);
            }
            OrchestratorEvent::RewindCompleted {
                conversation_epoch,
                report,
                history,
            } => {
                let checkpoints = Arc::clone(&snapshot.checkpoints);
                let sessions = Arc::clone(&snapshot.sessions);
                let current_session_id = snapshot.current_session_id.clone();
                let deployment = snapshot.deployment.clone();
                let reasoning_effort = snapshot.reasoning_effort;
                let context_budget = snapshot.context_budget;
                let max_context_budget = snapshot.max_context_budget;
                let github = snapshot.github.clone();
                let instructions = snapshot.instructions.clone();
                let skills = snapshot.skills.clone();
                let automation = snapshot.automation.clone();
                let plugins = snapshot.plugins.clone();
                let mcp_servers = Arc::clone(&snapshot.mcp_servers);
                let mcp_oauth_prompt = snapshot.mcp_oauth_prompt.clone();
                let lsp_servers = Arc::clone(&snapshot.lsp_servers);
                let lsp_diagnostics = Arc::clone(&snapshot.lsp_diagnostics);
                let code_index = snapshot.code_index.clone();
                let code_index_hits = Arc::clone(&snapshot.code_index_hits);
                let privacy = snapshot.privacy.clone();
                let subagents = snapshot.subagents.clone();
                let work_modes = snapshot.work_modes.clone();
                let auto_approval = snapshot.auto_approval;
                let reviews = snapshot.reviews.clone();
                let usage = snapshot.usage.clone();
                let side_chat = snapshot.side_chat.clone();
                let follow_ups = snapshot.follow_ups.clone();
                let paused_turn_id = snapshot.paused_turn_id;
                let whip = WhipTelemetry {
                    penalty_responses_remaining: 0,
                    ..snapshot.whip.clone()
                };
                let status = if report.preserved_conflicts.is_empty() {
                    format!(
                        "Rewound checkpoint #{}: {} file(s), {} history entries",
                        report.checkpoint_id,
                        report.restored_files.len(),
                        report.restored_history_entries
                    )
                } else {
                    format!(
                        "Rewound checkpoint #{}; preserved {} manually changed file(s)",
                        report.checkpoint_id,
                        report.preserved_conflicts.len()
                    )
                };
                *snapshot = UiSnapshot {
                    conversation_epoch: *conversation_epoch,
                    phase_revision: snapshot.phase_revision.saturating_add(1),
                    history_revision: snapshot.history_revision.saturating_add(1),
                    history: Arc::clone(history),
                    paused_turn_id,
                    usage,
                    whip,
                    side_chat,
                    follow_ups,
                    work_modes,
                    auto_approval,
                    reviews,
                    checkpoints,
                    sessions,
                    current_session_id,
                    deployment,
                    reasoning_effort,
                    context_budget,
                    max_context_budget,
                    github,
                    instructions,
                    skills,
                    automation,
                    plugins,
                    mcp_servers,
                    mcp_oauth_prompt,
                    lsp_servers,
                    lsp_diagnostics,
                    code_index,
                    code_index_hits,
                    privacy,
                    subagents,
                    status,
                    ..UiSnapshot::default()
                };
            }
            OrchestratorEvent::SessionsUpdated {
                sessions,
                current_session_id,
            } => {
                snapshot.sessions = Arc::clone(sessions);
                snapshot.current_session_id.clone_from(current_session_id);
            }
            OrchestratorEvent::SessionActivated {
                conversation_epoch,
                summary,
                history,
                usage,
                side_chat,
                follow_ups,
                paused_turn_id,
                context_budget,
            } => {
                let sessions = Arc::clone(&snapshot.sessions);
                let checkpoints = Arc::clone(&snapshot.checkpoints);
                let deployment = snapshot.deployment.clone();
                let reasoning_effort = snapshot.reasoning_effort;
                let max_context_budget = snapshot.max_context_budget;
                let github = snapshot.github.clone();
                let instructions = snapshot.instructions.clone();
                let skills = snapshot.skills.clone();
                let automation = snapshot.automation.clone();
                let plugins = snapshot.plugins.clone();
                let mcp_servers = Arc::clone(&snapshot.mcp_servers);
                let mcp_oauth_prompt = snapshot.mcp_oauth_prompt.clone();
                let lsp_servers = Arc::clone(&snapshot.lsp_servers);
                let lsp_diagnostics = Arc::clone(&snapshot.lsp_diagnostics);
                let code_index = snapshot.code_index.clone();
                let code_index_hits = Arc::clone(&snapshot.code_index_hits);
                let privacy = snapshot.privacy.clone();
                let subagents = snapshot.subagents.clone();
                let work_modes = snapshot.work_modes.clone();
                let auto_approval = snapshot.auto_approval;
                let reviews = snapshot.reviews.clone();
                let whip = WhipTelemetry {
                    penalty_responses_remaining: 0,
                    ..snapshot.whip.clone()
                };
                *snapshot = UiSnapshot {
                    conversation_epoch: *conversation_epoch,
                    phase_revision: snapshot.phase_revision.saturating_add(1),
                    history_revision: snapshot.history_revision.saturating_add(1),
                    history: Arc::clone(history),
                    usage: Some(usage.clone()),
                    whip,
                    side_chat: side_chat.clone(),
                    follow_ups: follow_ups.clone(),
                    work_modes,
                    auto_approval,
                    reviews,
                    sessions,
                    checkpoints,
                    current_session_id: Some(summary.id.clone()),
                    paused_turn_id: *paused_turn_id,
                    deployment,
                    reasoning_effort,
                    context_budget: *context_budget,
                    max_context_budget,
                    github,
                    instructions,
                    skills,
                    automation,
                    plugins,
                    mcp_servers,
                    mcp_oauth_prompt,
                    lsp_servers,
                    lsp_diagnostics,
                    code_index,
                    code_index_hits,
                    privacy,
                    subagents,
                    status: format!("Resumed session: {}", summary.title),
                    ..UiSnapshot::default()
                };
            }
            OrchestratorEvent::RuntimeSettingsUpdated {
                deployment,
                reasoning_effort,
                context_budget,
            } => {
                snapshot.deployment.clone_from(deployment);
                snapshot.reasoning_effort = *reasoning_effort;
                snapshot.context_budget = *context_budget;
                snapshot.status = format!(
                    "Runtime: {} / {} / {}K context",
                    deployment,
                    reasoning_effort,
                    context_budget / 1_000
                );
            }
            OrchestratorEvent::HistorySnapshot(history) => {
                snapshot.history_revision = snapshot.history_revision.saturating_add(1);
                snapshot.history = Arc::clone(history);
            }
            OrchestratorEvent::Usage { usage, .. } => snapshot.usage = Some(usage.clone()),
            OrchestratorEvent::RetryScheduled {
                turn_id,
                next_attempt,
                max_attempts,
                reason,
                ..
            } => {
                snapshot.active_turn_id = Some(*turn_id);
                snapshot.connection_status = match self.client.transport() {
                    crate::config::ApiTransport::WebSocket => "reconnecting WebSocket",
                    crate::config::ApiTransport::Auto | crate::config::ApiTransport::Sse => {
                        "reconnecting SSE"
                    }
                }
                .to_owned();
                snapshot.retry = Some(RetrySnapshot {
                    next_attempt: *next_attempt,
                    max_attempts: *max_attempts,
                    reason: reason.clone(),
                });
            }
            OrchestratorEvent::BusyRejected { message, .. }
            | OrchestratorEvent::RecoverableError { message, .. }
            | OrchestratorEvent::FatalError { message } => {
                snapshot.status.clone_from(message);
            }
            OrchestratorEvent::Done { .. } => {
                snapshot.active_turn_id = None;
                snapshot.paused_turn_id = None;
                snapshot.thinking.clear();
                snapshot.modal = None;
                snapshot.retry = None;
                snapshot.status = "Ready".to_owned();
            }
            OrchestratorEvent::TurnPaused { turn_id } => {
                snapshot.active_turn_id = None;
                snapshot.paused_turn_id = Some(*turn_id);
                snapshot.thinking.clear();
                snapshot.modal = None;
                snapshot.retry = None;
                snapshot.status = format!(
                    "Turn #{turn_id} paused at a durable boundary; Continue resumes it explicitly"
                );
            }
        }
        });
    }
}

fn remember_tool_action(
    actions: &mut Arc<BTreeMap<ActionId, ToolAction>>,
    action_id: ActionId,
    action: &ToolAction,
) {
    let actions = Arc::make_mut(actions);
    actions.insert(action_id, bounded_tool_action(action));
    while actions.len() > UI_TOOL_ACTION_MAX_ENTRIES {
        let Some(oldest) = actions.keys().next().copied() else {
            break;
        };
        actions.remove(&oldest);
    }
}

fn remember_mcp_call(
    calls: &mut Arc<BTreeMap<ActionId, Arc<McpToolCall>>>,
    action_id: ActionId,
    call: &Arc<McpToolCall>,
) {
    let calls = Arc::make_mut(calls);
    calls.insert(action_id, Arc::clone(call));
    while calls.len() > UI_TOOL_ACTION_MAX_ENTRIES {
        let Some(oldest) = calls.keys().next().copied() else {
            break;
        };
        calls.remove(&oldest);
    }
}

fn ui_history_entry_bytes(entry: &HistoryEntry) -> usize {
    let kind_bytes = match &entry.kind {
        HistoryKind::User | HistoryKind::Assistant => 16,
        HistoryKind::ToolResult { tool_name, .. } => 32_usize.saturating_add(tool_name.len()),
    };
    let summary_bytes = entry.tool_summary.as_ref().map_or(0, |summary| {
        summary
            .tool_name
            .len()
            .saturating_add(summary.target.as_ref().map_or(0, String::len))
    });
    128_usize
        .saturating_add(kind_bytes)
        .saturating_add(summary_bytes)
        .saturating_add(entry.content.len())
}

fn bounded_tool_action(action: &ToolAction) -> ToolAction {
    match action {
        ToolAction::ReadFile { path } => ToolAction::ReadFile {
            path: bounded_ui_fragment(path, UI_TOOL_ACTION_MAX_BYTES),
        },
        ToolAction::ListDirectory { path } => ToolAction::ListDirectory {
            path: bounded_ui_fragment(path, UI_TOOL_ACTION_MAX_BYTES),
        },
        ToolAction::SearchCode { pattern, path } => ToolAction::SearchCode {
            pattern: bounded_ui_fragment(pattern, UI_TOOL_ACTION_MAX_BYTES * 3 / 4),
            path: path
                .as_deref()
                .map(|value| bounded_ui_fragment(value, UI_TOOL_ACTION_MAX_BYTES / 4)),
        },
        ToolAction::ApplyPatch {
            path,
            search,
            replace,
        } => ToolAction::ApplyPatch {
            path: bounded_ui_fragment(path, UI_TOOL_ACTION_MAX_BYTES / 8),
            search: bounded_ui_fragment(search, UI_TOOL_ACTION_MAX_BYTES * 7 / 16),
            replace: bounded_ui_fragment(replace, UI_TOOL_ACTION_MAX_BYTES * 7 / 16),
        },
        ToolAction::WriteFile { path, content } => ToolAction::WriteFile {
            path: bounded_ui_fragment(path, UI_TOOL_ACTION_MAX_BYTES / 8),
            content: bounded_ui_fragment(content, UI_TOOL_ACTION_MAX_BYTES * 7 / 8),
        },
        ToolAction::ExecuteCommand {
            command,
            requires_confirmation,
        } => ToolAction::ExecuteCommand {
            command: bounded_ui_fragment(command, UI_TOOL_ACTION_MAX_BYTES),
            requires_confirmation: *requires_confirmation,
        },
    }
}

fn bounded_ui_fragment(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    const MARKER: &str = "\n[display artifact truncated]";
    let mut end = max_bytes.saturating_sub(MARKER.len()).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let mut bounded = String::with_capacity(end.saturating_add(MARKER.len()));
    bounded.push_str(&value[..end]);
    bounded.push_str(MARKER);
    bounded
}

fn truncate_front_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while !value.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    value.drain(..start);
}

fn utf8_tail(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut start = value.len().saturating_sub(max_bytes);
    while start < value.len() && !value.is_char_boundary(start) {
        start = start.saturating_add(1);
    }
    value.get(start..).unwrap_or_default()
}

/// Keep append-heavy live text bounded without shifting the retained buffer
/// for every one-byte SSE delta after the limit is reached. The slack makes
/// compaction amortized while the UI still sees a small, deterministic tail.
fn truncate_front_utf8_amortized(value: &mut String, retain_bytes: usize, compact_at: usize) {
    let compact_at = compact_at.max(retain_bytes);
    if value.len() <= compact_at {
        return;
    }
    truncate_front_utf8(value, retain_bytes);
}

fn truncate_back_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    if max_bytes == 0 {
        value.clear();
        return;
    }
    let suffix = "\n[… UI artifact truncated …]";
    let content_limit = max_bytes.saturating_sub(suffix.len());
    let mut end = content_limit.min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    if suffix.len() <= max_bytes {
        value.push_str(suffix);
    }
}

fn approximate_tokens(content: &str) -> u32 {
    u32::try_from(content.len())
        .unwrap_or(u32::MAX)
        .saturating_add(3)
        / 4
}

fn parse_native_arguments<T: DeserializeOwned>(arguments: &Value) -> Result<T, SubagentError> {
    serde_json::from_value(arguments.clone()).map_err(|error| {
        SubagentError::Protocol(format!("invalid native function arguments: {error}"))
    })
}

fn subagent_snapshot_json(snapshot: &SubagentSnapshot, include_detail: bool) -> Value {
    let recovery = snapshot.recovery.as_ref().map(|recovery| {
        let uncertain_action = recovery.uncertain_action.as_ref().map(|pending| {
            serde_json::json!({
                "action_id": pending.action_id,
                "tool": pending.action.tool_name(),
                "action": pending.action,
            })
        });
        serde_json::json!({
            "attempt": recovery.attempt,
            "checkpoint_at": recovery.checkpoint_at,
            "reason": recovery.reason,
            "can_resume": recovery.can_resume,
            "uncertain_action": uncertain_action,
        })
    });
    let mut value = serde_json::json!({
        "agent_id": snapshot.id.get(),
        "parent_id": snapshot.parent_id.map(SubagentId::get),
        "depth": snapshot.depth,
        "revision": snapshot.revision,
        "session_id": snapshot.session_id,
        "label": snapshot.label,
        "profile_id": snapshot.profile_id,
        "profile_name": snapshot.profile_name,
        "mode": snapshot.mode,
        "status": snapshot.status,
        "deployment": snapshot.deployment,
        "reasoning_effort": snapshot.reasoning_effort,
        "created_at": snapshot.created_at,
        "started_at": snapshot.started_at,
        "completed_at": snapshot.completed_at,
        "updated_at": snapshot.updated_at,
        "input_tokens": snapshot.input_tokens,
        "output_tokens": snapshot.output_tokens,
        "total_tokens": snapshot.total_tokens,
        "tool_iterations": snapshot.tool_iterations,
        "last_message": snapshot.last_message,
        "changed_files": snapshot.changed_files.iter().collect::<Vec<_>>(),
        "resolved_files": snapshot.resolved_files.iter().collect::<Vec<_>>(),
        "change_digest": snapshot.change_digest,
        "has_pending_command": snapshot.pending_command.is_some(),
        "recovery": recovery,
        "depends_on": snapshot.dependencies.iter().map(|id| id.get()).collect::<Vec<_>>(),
        "file_claims": snapshot.file_claims.iter().collect::<Vec<_>>(),
    });
    if include_detail && let Value::Object(object) = &mut value {
        object.insert("task".to_owned(), Value::String(snapshot.task.clone()));
        object.insert("result".to_owned(), Value::String(snapshot.result.clone()));
        object.insert(
            "error".to_owned(),
            snapshot.error.clone().map_or(Value::Null, Value::String),
        );
        object.insert(
            "transcript".to_owned(),
            Value::Array(
                snapshot
                    .transcript
                    .iter()
                    .map(|entry| {
                        serde_json::json!({
                            "at": entry.at,
                            "label": entry.label,
                            "content": entry.content,
                        })
                    })
                    .collect(),
            ),
        );
    }
    value
}

fn history_to_input(history: Vec<HistoryEntry>) -> InputItems {
    InputItems::from_opaque(super::state::repaired_replay_items(history))
}

fn terminal_text(
    final_text: Option<String>,
    deltas: String,
    response: &ResponsesResponse,
) -> String {
    let response_text = response.output_text();
    if !response_text.is_empty() {
        response_text
    } else if let Some(text) = final_text {
        text
    } else {
        deltas
    }
}

fn response_error(kind: &str, response: &ResponsesResponse) -> ApiError {
    if let Some(error) = &response.error {
        return ApiError::remote(error.code.as_deref(), &error.message);
    }
    ApiError::Protocol(format!(
        "response {kind} (id={}, status={:?})",
        response.id, response.status
    ))
}

fn turn_metrics_since(
    before: &UsageSnapshot,
    after: &UsageSnapshot,
    deployment: &str,
    elapsed: Duration,
) -> TurnMetrics {
    let deployment_cost = |snapshot: &UsageSnapshot| {
        snapshot
            .deployments
            .iter()
            .find(|item| item.deployment == deployment)
            .and_then(|item| item.cost_microusd)
    };
    TurnMetrics {
        elapsed_millis: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        input_tokens: after
            .usage
            .input_tokens
            .saturating_sub(before.usage.input_tokens),
        output_tokens: after
            .usage
            .output_tokens
            .saturating_sub(before.usage.output_tokens),
        total_tokens: after
            .usage
            .total_tokens
            .saturating_sub(before.usage.total_tokens),
        cost_microusd: deployment_cost(after)
            .map(|cost| cost.saturating_sub(deployment_cost(before).unwrap_or_default())),
    }
}

fn penalized_output_tokens(base: u32, whip: &WhipConfig) -> u32 {
    let scaled = u64::from(base).saturating_mul(u64::from(whip.max_output_percent)) / 100;
    u32::try_from(scaled)
        .unwrap_or(u32::MAX)
        .max(whip.minimum_output_tokens.min(base))
        .min(base)
}

#[cfg(test)]
mod tests {
    use super::{
        AwaitedControl, LIST_SKILL_RESOURCES_TOOL, Orchestrator, ProtocolBoundaryTracker,
        READ_SKILL_RESOURCE_TOOL, READ_SKILL_TOOL, REVIEW_DIFF_TOOL, SPAWN_AGENT_TOOL,
        SUBMIT_REVIEW_TOOL, UI_TOOL_ACTION_MAX_BYTES, UPDATE_GOAL_TOOL, UrgentControlHandle,
        UrgentControlKind, bounded_tool_action, collect_parallel_read_batch,
        explore_allows_native_function, goal_update_function_definition, history_to_input,
        penalized_output_tokens, subagent_function_definitions, truncate_front_utf8_amortized,
    };
    use crate::{
        agent::state::{AgentState, HistoryKind, ToolResultStatus},
        agent::{
            FollowUpMode, ShellApprovalDecision, automation::AutomationCatalog,
            review::DiffSnapshot,
        },
        api::{FunctionCall, ReasoningEffort},
        config::{
            AgentConfig, ApiAuth, ApiConfig, ApiProvider, ContextMode, ProjectInstructionsConfig,
            ResponsesEndpoint, ShellConfig, SkillsConfig, SubagentConfig, WhipConfig,
        },
        parser::{ParserEvent, ToolAction, ToolOutcome},
        tools::{
            ApprovalBinding, ApprovalNonce, CommandDigest, ConfirmationDecision, ConfirmationReason,
        },
    };
    use secrecy::SecretString;
    use std::{
        collections::VecDeque,
        fs, io,
        path::Path,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn boundary_tracker_handles_closing_tag_split_around_strike() {
        let mut tracker = ProtocolBoundaryTracker::default();
        assert!(!tracker.feed("before<thinking>x</think"));
        assert!(!tracker.is_at_boundary());
        assert!(tracker.feed("ing>tail"));
        assert!(tracker.is_at_boundary());
    }

    #[test]
    fn read_batches_stop_at_parse_and_mutation_boundaries() {
        let mut pending = VecDeque::from([
            ParserEvent::ToolCallParsed(ToolAction::ListDirectory {
                path: "src".to_owned(),
            }),
            ParserEvent::ToolCallParsed(ToolAction::SearchCode {
                pattern: "needle".to_owned(),
                path: None,
            }),
            ParserEvent::ToolCallParseError {
                raw_tag: "<read_file>".to_owned(),
                reason: "unclosed".to_owned(),
            },
            ParserEvent::ToolCallParsed(ToolAction::ReadFile {
                path: "later.rs".to_owned(),
            }),
        ]);
        let batch = collect_parallel_read_batch(
            ToolAction::ReadFile {
                path: "first.rs".to_owned(),
            },
            &mut pending,
        );
        assert_eq!(batch.len(), 3);
        assert!(matches!(
            pending.front(),
            Some(ParserEvent::ToolCallParseError { .. })
        ));

        let mut mutation = VecDeque::from([
            ParserEvent::ToolCallParsed(ToolAction::WriteFile {
                path: "out.rs".to_owned(),
                content: "changed".to_owned(),
            }),
            ParserEvent::ToolCallParsed(ToolAction::ReadFile {
                path: "after.rs".to_owned(),
            }),
        ]);
        let batch = collect_parallel_read_batch(
            ToolAction::ReadFile {
                path: "before.rs".to_owned(),
            },
            &mut mutation,
        );
        assert_eq!(batch.len(), 1);
        assert!(matches!(
            mutation.front(),
            Some(ParserEvent::ToolCallParsed(ToolAction::WriteFile { .. }))
        ));

        let mut oversized = VecDeque::from(
            (1..=5)
                .map(|index| {
                    ParserEvent::ToolCallParsed(ToolAction::ReadFile {
                        path: format!("{index}.rs"),
                    })
                })
                .collect::<Vec<_>>(),
        );
        let batch = collect_parallel_read_batch(
            ToolAction::ReadFile {
                path: "zero.rs".to_owned(),
            },
            &mut oversized,
        );
        assert_eq!(batch.len(), 4);
        assert_eq!(oversized.len(), 2);
    }

    #[test]
    fn subagent_native_tools_are_strict_and_sequential() -> Result<(), serde_json::Error> {
        let definitions = subagent_function_definitions(&[
            "builtin:research".to_owned(),
            "builtin:writer".to_owned(),
        ]);
        let names = definitions
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "spawn_agent",
                "list_agents",
                "get_agent",
                "send_agent_message",
                "interrupt_agent",
                "wait_agent",
            ]
        );
        for definition in &definitions {
            let value = serde_json::to_value(definition)?;
            assert_eq!(value["strict"], true);
            assert_eq!(value["parameters"]["additionalProperties"], false);
        }
        let spawn = serde_json::to_value(&definitions[0])?;
        let required = spawn["parameters"]["required"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for field in ["task", "profile_id", "depends_on", "file_claims"] {
            assert!(required.iter().any(|value| value == field));
        }
        assert!(
            spawn["parameters"]["properties"]["depends_on"]
                .get("uniqueItems")
                .is_none()
        );
        assert!(
            spawn["parameters"]["properties"]["file_claims"]
                .get("uniqueItems")
                .is_none()
        );

        let without_profiles = subagent_function_definitions(&[]);
        assert!(
            without_profiles
                .iter()
                .all(|definition| definition.name != SPAWN_AGENT_TOOL)
        );
        Ok(())
    }

    #[test]
    fn goal_update_tool_is_strict_and_requires_verification() -> Result<(), serde_json::Error> {
        let value = serde_json::to_value(goal_update_function_definition())?;
        assert_eq!(value["name"], "update_goal");
        assert_eq!(value["strict"], true);
        assert_eq!(value["parameters"]["additionalProperties"], false);
        assert!(
            value["parameters"]["required"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item == "verification"))
        );
        Ok(())
    }

    #[test]
    fn stateful_follow_up_keeps_native_function_output_as_an_input_item()
    -> Result<(), serde_json::Error> {
        let mut state = AgentState::new();
        state.push_user(1, "inspect");
        let sequence = state.push_tool_diagnostic(
            1,
            9,
            "mcp:files::read",
            ToolResultStatus::Success,
            "display copy",
        );
        assert!(state.set_api_items(
            sequence,
            vec![serde_json::json!({
                "type": "function_call_output",
                "call_id": "call_stateful_9",
                "output": "{\"ok\":true}",
            })],
        ));
        let input = history_to_input(state.request_context_after(1));
        let value = serde_json::to_value(input)?;
        assert_eq!(value[0]["type"], "function_call_output");
        assert_eq!(value[0]["call_id"], "call_stateful_9");
        assert!(value[0].get("role").is_none());
        Ok(())
    }

    #[test]
    fn boundary_tracker_is_immediately_safe_outside_protocol() {
        let mut tracker = ProtocolBoundaryTracker::default();
        assert!(!tracker.feed("ordinary assistant text with 2 < 3 and <unknown>"));
        assert!(tracker.is_at_boundary());

        let mut partial_plaintext = ProtocolBoundaryTracker::default();
        assert!(!partial_plaintext.feed("ordinary less-than: <"));
        assert!(partial_plaintext.is_at_boundary());

        let mut partial_opening = ProtocolBoundaryTracker::default();
        assert!(!partial_opening.feed("<think"));
        assert!(partial_opening.is_at_boundary());
        assert!(!partial_opening.feed("ing>inside"));
        assert!(!partial_opening.is_at_boundary());
    }

    #[test]
    fn boundary_tracker_ignores_nested_tool_fields_but_waits_for_outer_close() {
        let mut tracker = ProtocolBoundaryTracker::default();
        assert!(!tracker.feed("<write_file><path>x</path><content>y</content>"));
        assert!(!tracker.is_at_boundary());
        assert!(tracker.feed("</write_file>"));
        assert!(tracker.is_at_boundary());
    }

    #[test]
    fn whip_penalty_scales_and_clamps() {
        let config = WhipConfig {
            enabled: true,
            hotkey: "w".to_owned(),
            double_hit_window: Duration::from_secs(2),
            penalty_completed_responses: 3,
            max_output_percent: 60,
            minimum_output_tokens: 256,
        };
        assert_eq!(penalized_output_tokens(1_000, &config), 600);
        assert_eq!(penalized_output_tokens(100, &config), 100);
    }

    #[tokio::test]
    async fn new_session_drops_remaining_whip_penalty() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.penalty_responses_remaining = 2;

        assert!(orchestrator.start_new_session().await);
        assert_eq!(orchestrator.penalty_responses_remaining, 0);
        Ok(())
    }

    #[tokio::test]
    async fn context_budget_is_session_scoped_while_new_sessions_use_the_latest_choice()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        assert!(orchestrator.ensure_current_session("first").await);
        let first = orchestrator
            .current_session_id
            .clone()
            .ok_or("first session was not created")?;
        assert!(
            orchestrator
                .update_runtime_settings(
                    "test-model".to_owned(),
                    ReasoningEffort::Medium,
                    false,
                    300_000,
                )
                .await
        );

        assert!(orchestrator.start_new_session().await);
        assert_eq!(orchestrator.agent_config.context_budget, 300_000);
        assert!(orchestrator.ensure_current_session("second").await);
        let second = orchestrator
            .current_session_id
            .clone()
            .ok_or("second session was not created")?;
        assert!(
            orchestrator
                .update_runtime_settings(
                    "test-model".to_owned(),
                    ReasoningEffort::Medium,
                    false,
                    100_000,
                )
                .await
        );

        assert!(orchestrator.resume_session(first.clone(), true).await);
        assert_eq!(orchestrator.agent_config.context_budget, 300_000);
        assert_eq!(snapshots.borrow().context_budget, 300_000);

        assert!(orchestrator.resume_session(second, true).await);
        assert_eq!(orchestrator.agent_config.context_budget, 100_000);
        assert_eq!(snapshots.borrow().context_budget, 100_000);

        assert!(orchestrator.resume_session(first.clone(), true).await);
        assert!(orchestrator.start_new_session().await);
        assert_eq!(orchestrator.agent_config.context_budget, 100_000);
        assert_eq!(snapshots.borrow().context_budget, 100_000);

        drop(orchestrator);
        let (api, mut agent) = test_configs(root.path());
        agent.context_budget = 100_000;
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut restarted, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        assert!(restarted.resume_session(first, true).await);
        assert_eq!(restarted.agent_config.context_budget, 300_000);
        assert_eq!(snapshots.borrow().context_budget, 300_000);
        assert!(restarted.start_new_session().await);
        assert_eq!(restarted.agent_config.context_budget, 100_000);
        assert_eq!(snapshots.borrow().context_budget, 100_000);
        Ok(())
    }

    #[tokio::test]
    async fn resuming_an_older_session_keeps_the_conversation_epoch_monotonic()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let mut older = AgentState::new();
        older.push_user(1, "older session");
        let document = orchestrator
            .session_store
            .create("older session".to_owned(), older, None)
            .await?;
        orchestrator.conversation_epoch = 40;
        orchestrator.state.conversation_epoch = 40;
        orchestrator.snapshot_tx.send_modify(|snapshot| {
            snapshot.conversation_epoch = 40;
            snapshot.phase = crate::agent::phase::AgentPhase::Idle;
        });

        assert!(orchestrator.resume_session(document.summary.id, true).await);

        let snapshot = orchestrator.snapshot_tx.borrow();
        assert!(snapshot.conversation_epoch > 40);
        assert_eq!(orchestrator.conversation_epoch, snapshot.conversation_epoch);
        Ok(())
    }

    #[test]
    fn idle_session_navigation_tolerates_a_lagging_phase_revision()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.snapshot_tx.send_modify(|snapshot| {
            snapshot.phase = crate::agent::phase::AgentPhase::Idle;
            snapshot.phase_revision = 7;
        });
        let snapshot = orchestrator.snapshot_tx.borrow();
        let scope = super::CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: 6,
        };
        drop(snapshot);

        assert!(orchestrator.accepts_session_navigation_scope(scope));
        Ok(())
    }

    #[test]
    fn paused_and_failed_turns_do_not_lock_session_navigation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.retryable_turn = Some(7);

        for phase in [
            crate::agent::phase::AgentPhase::Idle,
            crate::agent::phase::AgentPhase::Error {
                message: "network".to_owned(),
                recoverable: true,
            },
        ] {
            orchestrator.snapshot_tx.send_modify(|snapshot| {
                snapshot.phase = phase.clone();
                snapshot.phase_revision = snapshot.phase_revision.saturating_add(1);
            });
            let snapshot = orchestrator.snapshot_tx.borrow();
            let scope = super::CommandScope {
                conversation_epoch: snapshot.conversation_epoch,
                phase_revision: snapshot.phase_revision,
            };
            drop(snapshot);
            assert!(orchestrator.accepts_session_navigation_scope(scope));
        }
        Ok(())
    }

    #[tokio::test]
    async fn leaving_a_failed_session_preserves_the_turn_as_paused()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_pending_user(7, "unfinished work");
        orchestrator.state.mark_turn_paused(7);
        orchestrator.state.begin_turn(7);
        orchestrator.retryable_turn = Some(7);
        assert!(orchestrator.ensure_current_session("unfinished work").await);
        let session_id = orchestrator
            .current_session_id
            .clone()
            .ok_or("session was not created")?;

        assert!(orchestrator.prepare_session_transition().await);

        assert_eq!(orchestrator.state.paused_turn_id, Some(7));
        assert_eq!(orchestrator.state.in_flight_turn_id, None);
        let persisted = orchestrator.session_store.load(session_id, true).await?;
        assert_eq!(persisted.state.paused_turn_id, Some(7));
        assert!(matches!(
            persisted.state.history[0].status,
            crate::agent::state::HistoryStatus::Paused
        ));
        Ok(())
    }

    #[tokio::test]
    async fn reopening_the_current_failed_session_keeps_its_paused_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_pending_user(7, "unfinished work");
        orchestrator.retryable_turn = Some(7);
        assert!(orchestrator.ensure_current_session("unfinished work").await);
        let session_id = orchestrator
            .current_session_id
            .clone()
            .ok_or("session was not created")?;

        assert!(orchestrator.resume_session(session_id, true).await);

        assert_eq!(orchestrator.state.paused_turn_id, Some(7));
        assert!(matches!(
            orchestrator.state.history[0].status,
            crate::agent::state::HistoryStatus::Paused
        ));
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_persists_an_active_turn_as_paused() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator
            .state
            .push_pending_user(9, "unfinished command");
        orchestrator.state.push_assistant(9, "running tool");
        orchestrator.state.mark_turn_committed(9);
        orchestrator.state.begin_turn(9);
        assert!(
            orchestrator
                .ensure_current_session("unfinished command")
                .await
        );
        let session_id = orchestrator
            .current_session_id
            .clone()
            .ok_or("session was not created")?;

        orchestrator.prepare_shutdown(9).await;

        assert_eq!(orchestrator.state.paused_turn_id, Some(9));
        let persisted = orchestrator.session_store.load(session_id, true).await?;
        assert_eq!(persisted.state.paused_turn_id, Some(9));
        assert_eq!(persisted.state.in_flight_turn_id, None);
        Ok(())
    }

    #[tokio::test]
    async fn reset_snapshot_uses_the_authoritative_session_identity()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (orchestrator, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let stale = crate::agent::persistence::SessionId::parse("stale-session")?;
        orchestrator.snapshot_tx.send_modify(|snapshot| {
            snapshot.current_session_id = Some(stale);
        });

        orchestrator.update_snapshot(&super::OrchestratorEvent::ResetAcknowledged {
            conversation_epoch: 2,
        });

        assert!(snapshots.borrow().current_session_id.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn conversation_reset_keeps_runtime_snapshots() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let privacy = snapshots.borrow().privacy.clone();
        assert!(!privacy.sources.is_empty());
        let oauth_prompt = crate::mcp::McpOAuthPrompt {
            server: "docs".to_owned(),
            authorization_url: "https://example.test/authorize".to_owned(),
            redirect_uri: "http://127.0.0.1/callback".to_owned(),
            browser_opened: false,
        };
        orchestrator.snapshot_tx.send_modify(|snapshot| {
            snapshot.mcp_oauth_prompt = Some(oauth_prompt.clone());
        });

        orchestrator.reset_state().await;

        let snapshot = snapshots.borrow();
        assert_eq!(snapshot.privacy, privacy);
        assert_eq!(snapshot.mcp_oauth_prompt.as_ref(), Some(&oauth_prompt));
        Ok(())
    }

    #[tokio::test]
    async fn rewind_keeps_session_data_that_is_not_rewound()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (orchestrator, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.snapshot_tx.send_modify(|snapshot| {
            snapshot.side_chat.revision = 7;
            snapshot.follow_ups.revision = 9;
            snapshot.usage = Some(crate::usage::UsageSnapshot::default());
            snapshot.whip.total_strikes = 3;
            snapshot.whip.estimated_saved_token_budget = 1_024;
        });

        orchestrator.update_snapshot(&super::OrchestratorEvent::RewindCompleted {
            conversation_epoch: 2,
            report: super::RewindReport {
                checkpoint_id: 1,
                restored_files: Vec::new(),
                preserved_conflicts: Vec::new(),
                discarded_checkpoints: 0,
                restored_history_entries: 0,
            },
            history: Arc::from([]),
        });

        let snapshot = snapshots.borrow();
        assert_eq!(snapshot.side_chat.revision, 7);
        assert_eq!(snapshot.follow_ups.revision, 9);
        assert!(snapshot.usage.is_some());
        assert_eq!(snapshot.whip.total_strikes, 3);
        assert_eq!(snapshot.whip.estimated_saved_token_budget, 1_024);
        Ok(())
    }

    #[tokio::test]
    async fn rejected_goal_update_is_not_reported_as_success()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, snapshots, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        let result = orchestrator
            .run_goal_update(
                1,
                FunctionCall {
                    call_id: "goal-call".to_owned(),
                    name: UPDATE_GOAL_TOOL.to_owned(),
                    arguments: "{}".to_owned(),
                },
            )
            .await;

        assert!(result.is_ok());
        assert!(snapshots.borrow().status.contains("failed"));
        Ok(())
    }

    #[tokio::test]
    async fn idle_urgent_reset_finalizes_the_pending_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let initialized = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()?;
        assert!(initialized.success());
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshots, urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let state_before = orchestrator.state.clone();
        let pending = orchestrator
            .checkpoint_store
            .as_mut()
            .ok_or_else(|| io::Error::other("Git workspace must enable checkpoints"))?
            .begin("pending turn", &state_before, None)
            .await?;
        orchestrator.pending_checkpoint = Some(pending);

        urgent.reset();
        assert!(orchestrator.handle_idle_urgent().await);

        assert!(orchestrator.pending_checkpoint.is_none());
        Ok(())
    }

    #[test]
    fn shutdown_and_reset_cannot_be_evicted_by_urgent_signal_floods() {
        let shutdown = UrgentControlHandle::default();
        shutdown.shutdown();
        for turn_id in 1..=32 {
            shutdown.whip(turn_id);
            shutdown.interrupt(turn_id);
        }
        let drained = shutdown.drain();
        assert!(
            matches!(drained.as_slice(), [signal] if matches!(signal.kind, UrgentControlKind::Shutdown))
        );

        let reset = UrgentControlHandle::default();
        reset.reset();
        for turn_id in 1..=32 {
            reset.whip(turn_id);
            reset.interrupt(turn_id);
        }
        let drained = reset.drain();
        assert!(
            matches!(drained.as_slice(), [signal] if matches!(signal.kind, UrgentControlKind::Reset))
        );
    }

    #[test]
    fn explicit_pause_cancels_the_active_turn_and_is_consumed_once() {
        let control = UrgentControlHandle::default();
        let cancellation = CancellationToken::new();
        control.activate_turn(73, cancellation.clone());

        assert!(control.pause(73) > 0);
        assert!(cancellation.is_cancelled());
        assert!(
            control.drain().iter().any(|signal| {
                matches!(signal.kind, UrgentControlKind::Interrupt { turn_id: 73 })
            })
        );
        assert!(control.take_pause_request(73));
        assert!(!control.take_pause_request(73));
        control.clear_turn(73);
    }

    #[test]
    fn pause_resume_note_keeps_visible_draft_but_drops_incomplete_protocol()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator
            .state
            .push_pending_user(17, "continue carefully");
        orchestrator.state.push_interrupted_assistant(
            17,
            "Visible draft. <thinking>private chain</thinking><execute_command><command>remove files",
        );
        orchestrator.pause_resume_note_pending = Some(17);

        let note = orchestrator
            .pause_resume_note(17)
            .ok_or("missing pause resume note")?;
        assert!(note.contains("Visible draft."));
        assert!(!note.contains("private chain"));
        assert!(!note.contains("remove files"));
        let request = serde_json::to_value(orchestrator.build_request(17, false)?)?;
        let encoded = request.to_string();
        assert!(encoded.contains("explicitly resumed"));
        assert!(encoded.contains("Visible draft"));
        Ok(())
    }

    #[test]
    fn snapshot_tool_artifact_is_bounded_without_slicing_utf8() {
        let action = ToolAction::WriteFile {
            path: "unicode.txt".to_owned(),
            content: "🦀".repeat(UI_TOOL_ACTION_MAX_BYTES),
        };
        let ToolAction::WriteFile { content, .. } = bounded_tool_action(&action) else {
            return;
        };
        assert!(content.len() <= UI_TOOL_ACTION_MAX_BYTES);
        assert!(content.ends_with("[display artifact truncated]"));
    }

    #[test]
    fn live_thinking_compaction_is_bounded_and_utf8_safe_with_tiny_deltas() {
        let mut thinking = String::new();
        for _ in 0..200_000 {
            thinking.push('Ж');
            truncate_front_utf8_amortized(&mut thinking, 64 * 1024, 128 * 1024);
        }
        assert!(thinking.len() <= 128 * 1024);
        assert!(thinking.chars().all(|character| character == 'Ж'));
    }

    fn test_configs(root: &Path) -> (ApiConfig, AgentConfig) {
        (
            ApiConfig {
                provider: ApiProvider::Azure,
                auth: ApiAuth::ApiKey,
                api_key: SecretString::new("test-key".to_owned().into()),
                bedrock_runtime: crate::config::BedrockRuntimeConfig::default(),
                transport: crate::config::ApiTransport::Sse,
                endpoint: ResponsesEndpoint::FullUrl("http://127.0.0.1:1/responses".to_owned()),
                allow_insecure_loopback: true,
                deployment: "test-model".to_owned(),
                deployment_choices: vec!["test-model".to_owned()],
                api_version: None,
                max_output_tokens: 512,
                reasoning_effort: ReasoningEffort::Medium,
                temperature: None,
                server_compaction_threshold: None,
                request_timeout: Duration::from_secs(1),
                stream_idle_timeout: Duration::from_secs(1),
                max_attempts: 1,
                retry_min_delay: Duration::from_millis(1),
                retry_max_delay: Duration::from_millis(1),
                retry_after_cap: Duration::from_secs(120),
                pricing: crate::usage::PricingCatalog::default(),
                pricing_catalog_url: None,
            },
            AgentConfig {
                context_mode: ContextMode::Stateless,
                context_budget: 8_192,
                max_context_budget: 2_000_000,
                max_tool_iterations: 4,
                workspace_root: root.to_path_buf(),
                session_dir: root.join(".test-sessions"),
                privacy_user_rules_file: root.join(".test-privacy.ignore"),
                instructions_file: root.join("instructions.md"),
                instructions: "test instructions".to_owned(),
                project_instructions: ProjectInstructionsConfig::default(),
                skills: SkillsConfig {
                    enabled: false,
                    ..SkillsConfig::default()
                },
                exec_timeout: Duration::from_secs(1),
                subagents: SubagentConfig {
                    enabled: false,
                    allow_mcp: false,
                    worktree_dir: root.join(".test-worktrees"),
                    max_parallel: 1,
                    max_per_session: 1,
                    max_tool_iterations: 1,
                    max_tokens_per_agent: 150_000,
                    max_total_tokens_per_session: 500_000,
                    max_depth: 3,
                    max_children_per_agent: 4,
                    task_timeout: Duration::from_secs(1),
                    git_timeout: Duration::from_secs(1),
                },
                shell: ShellConfig::default(),
                whip: WhipConfig::default(),
            },
        )
    }

    #[test]
    fn work_modes_raise_wire_effort_and_plan_request_has_no_tools()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_pending_user(1, "implement safely");
        orchestrator.state.work_modes.plan = true;

        let ordinary = serde_json::to_value(orchestrator.build_request(1, true)?)?;
        assert_eq!(ordinary["reasoning"]["effort"], "xhigh");
        assert!(
            ordinary["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("PLAN MODE IS ACTIVE"))
        );
        assert!(
            ordinary["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("# DEcode GPT coding profile")
                    && value.contains("<apply_patch>"))
        );
        assert!(
            ordinary["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("Read batching policy"))
        );

        let (plan, effort, mode) = orchestrator.build_plan_request()?;
        let plan = serde_json::to_value(plan)?;
        assert_eq!(effort, ReasoningEffort::XHigh);
        assert_eq!(mode, None);
        assert!(plan.get("tools").is_none());
        assert!(plan.get("previous_response_id").is_none());
        assert_eq!(plan["store"], false);
        assert!(
            plan["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("harness-enforced read-only"))
        );

        orchestrator.state.work_modes.deep_thinking = true;
        let deep = serde_json::to_value(orchestrator.build_request(1, true)?)?;
        assert_eq!(deep["reasoning"]["effort"], "max");
        assert_eq!(deep["reasoning"]["mode"], "pro");
        Ok(())
    }

    #[test]
    fn non_gpt_provider_keeps_its_existing_prompt_without_gpt_profile()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (mut api, agent) = test_configs(root.path());
        api.provider = ApiProvider::Google;
        api.auth = ApiAuth::GoogleKey;
        api.endpoint = ResponsesEndpoint::FullUrl("http://127.0.0.1:1/v1beta/models".to_owned());
        api.deployment = "gemini-3.1-pro-preview".to_owned();
        api.deployment_choices = vec![api.deployment.clone()];
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        let instructions = orchestrator.effective_instructions();
        assert!(instructions.starts_with("test instructions"));
        assert!(!instructions.contains("# DEcode GPT coding profile"));
        Ok(())
    }

    #[test]
    fn side_question_request_is_stateless_input_only_and_has_no_tools()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_user(1, "main task context");

        let request = serde_json::to_value(orchestrator.build_side_question_request(
            "Does this invariant still hold?",
            "review-model",
            ReasoningEffort::High,
        )?)?;

        assert_eq!(request["model"], "review-model");
        assert_eq!(request["store"], false);
        assert_eq!(request["reasoning"]["effort"], "high");
        assert!(request.get("previous_response_id").is_none());
        assert!(request.get("tools").is_none());
        assert!(
            request["instructions"]
                .as_str()
                .is_some_and(|instructions| {
                    instructions.contains("SIDE QUESTION CHANNEL")
                        && instructions.contains("MUST NOT call or imitate any tool")
                })
        );
        let input = request["input"].as_array().ok_or("missing input array")?;
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["content"], "main task context");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"], "Does this invariant still hold?");
        assert_eq!(orchestrator.state.history.len(), 1);
        Ok(())
    }

    #[test]
    fn explore_mode_is_xhigh_and_exposes_only_read_only_native_families()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_pending_user(1, "inspect only");
        orchestrator.state.work_modes.explore = true;

        let request = serde_json::to_value(orchestrator.build_request(1, true)?)?;
        assert_eq!(request["reasoning"]["effort"], "xhigh");
        assert!(
            request["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("EXPLORE MODE IS ACTIVE"))
        );
        assert!(request.get("tools").is_none());
        assert!(explore_allows_native_function("lsp_status"));
        assert!(explore_allows_native_function("codebase_search"));
        assert!(explore_allows_native_function(READ_SKILL_TOOL));
        assert!(explore_allows_native_function(UPDATE_GOAL_TOOL));
        assert!(!explore_allows_native_function(SPAWN_AGENT_TOOL));
        assert!(!explore_allows_native_function("mcp__git__commit"));
        Ok(())
    }

    #[tokio::test]
    async fn explore_mode_rejects_mutating_legacy_actions_before_execution_and_hooks()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        std::fs::write(root.path().join("owned.txt"), "manual content")?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.work_modes.explore = true;

        orchestrator
            .run_action(
                1,
                ToolAction::WriteFile {
                    path: "owned.txt".to_owned(),
                    content: "agent content".to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| format!("unexpected turn exit: {exit:?}"))?;

        assert_eq!(
            std::fs::read_to_string(root.path().join("owned.txt"))?,
            "manual content"
        );
        assert!(orchestrator.state.history.last().is_some_and(|entry| {
            entry
                .content
                .contains("blocked by the active Explore/Review")
                && matches!(
                    entry.kind,
                    HistoryKind::ToolResult {
                        outcome: ToolResultStatus::Failure,
                        ..
                    }
                )
        }));
        Ok(())
    }

    #[tokio::test]
    async fn explore_mode_rejects_unadvertised_native_calls_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.work_modes.explore = true;

        orchestrator
            .run_mcp_call(
                1,
                FunctionCall {
                    call_id: "call_blocked".to_owned(),
                    name: "mcp__git__commit".to_owned(),
                    arguments: "{}".to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| format!("unexpected turn exit: {exit:?}"))?;

        assert!(orchestrator.state.history.last().is_some_and(|entry| {
            entry
                .content
                .contains("blocked by the active Explore/Review")
                && matches!(
                    entry.kind,
                    HistoryKind::ToolResult {
                        outcome: ToolResultStatus::Failure,
                        ..
                    }
                )
        }));
        Ok(())
    }

    #[tokio::test]
    async fn review_mode_pages_immutable_diff_and_records_structured_report()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator
            .state
            .push_pending_user(4, "review current changes");
        orchestrator.state.work_modes.review = true;
        orchestrator.active_review = Some(DiffSnapshot {
            sha256: "a".repeat(64),
            changed_paths: vec!["src/lib.rs".to_owned()],
            diff: "diff --git a/src/lib.rs b/src/lib.rs\n+unsafe change\n".to_owned(),
        });

        let request = serde_json::to_value(orchestrator.build_request(4, true)?)?;
        assert_eq!(request["reasoning"]["effort"], "xhigh");
        assert!(
            request["instructions"]
                .as_str()
                .is_some_and(|value| value.contains("REVIEW MODE IS ACTIVE"))
        );
        let tool_names = request["tools"]
            .as_array()
            .ok_or("missing review tools")?
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(tool_names.contains(&REVIEW_DIFF_TOOL));
        assert!(tool_names.contains(&SUBMIT_REVIEW_TOOL));
        assert!(!tool_names.contains(&SPAWN_AGENT_TOOL));

        orchestrator
            .run_review_call(
                4,
                FunctionCall {
                    call_id: "call_diff".to_owned(),
                    name: REVIEW_DIFF_TOOL.to_owned(),
                    arguments: serde_json::json!({ "offset": 0, "max_bytes": 1024 }).to_string(),
                },
            )
            .await
            .map_err(|exit| format!("unexpected review diff exit: {exit:?}"))?;
        assert!(orchestrator.state.history.last().is_some_and(|entry| {
            entry.content.contains("unsafe change") && entry.content.contains("next_offset")
        }));

        orchestrator
            .run_review_call(
                4,
                FunctionCall {
                    call_id: "call_submit".to_owned(),
                    name: SUBMIT_REVIEW_TOOL.to_owned(),
                    arguments: serde_json::json!({
                        "snapshot_sha256": "a".repeat(64),
                        "verdict": "changes_requested",
                        "summary": "One correctness issue",
                        "findings": [{
                            "severity": "high",
                            "title": "Unsafe change",
                            "body": "The branch can corrupt state.",
                            "path": "src/lib.rs",
                            "line_start": 1,
                            "line_end": 1,
                            "suggested_fix": "Restore the invariant and add a test."
                        }]
                    })
                    .to_string(),
                },
            )
            .await
            .map_err(|exit| format!("unexpected submit review exit: {exit:?}"))?;
        assert!(orchestrator.state.reviews.submitted_for_turn(4));
        assert_eq!(snapshot_rx.borrow().reviews.open_findings(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn steer_declines_every_unexecuted_action_before_injecting_user_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.follow_ups.enqueue(
            FollowUpMode::Steer,
            "Do not touch disk; explain first".to_owned(),
            Some(5),
        )?;
        let mut native = VecDeque::from([FunctionCall {
            call_id: "call_never_run".to_owned(),
            name: "mcp__danger__write".to_owned(),
            arguments: "{}".to_owned(),
        }]);
        let mut legacy = VecDeque::from([crate::parser::ParserEvent::ToolCallParsed(
            ToolAction::WriteFile {
                path: "must-not-exist.txt".to_owned(),
                content: "unsafe".to_owned(),
            },
        )]);

        assert!(
            orchestrator
                .deliver_pending_steer(5, &mut native, &mut legacy)
                .await
                .map_err(|exit| format!("unexpected turn exit: {exit:?}"))?
        );
        assert!(native.is_empty());
        assert!(legacy.is_empty());
        assert!(!root.path().join("must-not-exist.txt").exists());
        assert_eq!(
            orchestrator.state.follow_ups.snapshot().items[0].status,
            crate::agent::FollowUpStatus::Delivered
        );
        assert!(orchestrator.state.history.iter().any(|entry| {
            entry.api_items.iter().any(|item| {
                item["type"] == "function_call_output" && item["call_id"] == "call_never_run"
            })
        }));
        assert!(orchestrator.state.history.iter().any(|entry| {
            matches!(entry.kind, HistoryKind::User)
                && entry.content.contains("Do not touch disk; explain first")
        }));
        Ok(())
    }

    #[test]
    fn project_instruction_hierarchy_is_bounded_into_wire_instructions_and_toggleable()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        std::fs::create_dir_all(root.path().join("frontend"))?;
        std::fs::write(root.path().join("AGENTS.md"), "root repository rule")?;
        std::fs::write(
            root.path().join("frontend/AGENTS.md"),
            "frontend-specific rule",
        )?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.push_pending_user(1, "inspect frontend");

        let enabled = serde_json::to_value(orchestrator.build_request(1, true)?)?;
        let enabled_instructions = enabled["instructions"].as_str().unwrap_or_default();
        assert!(enabled_instructions.contains("root repository rule"));
        assert!(enabled_instructions.contains("frontend-specific rule"));
        assert!(enabled_instructions.contains("scope: frontend"));

        orchestrator.instructions.set_project_enabled(false);
        let disabled = serde_json::to_value(orchestrator.build_request(1, true)?)?;
        let disabled_instructions = disabled["instructions"].as_str().unwrap_or_default();
        assert!(!disabled_instructions.contains("root repository rule"));
        assert!(!disabled_instructions.contains("frontend-specific rule"));
        Ok(())
    }

    #[tokio::test]
    async fn skills_preload_only_metadata_and_read_body_through_strict_native_tool()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let user_skills = tempdir()?;
        let skill_root = root.path().join(".decode/skills/review");
        std::fs::create_dir_all(&skill_root)?;
        std::fs::write(
            skill_root.join("SKILL.md"),
            "---\nname: Careful review\ndescription: Inspect cancellation paths\n---\n# Private body loaded on demand\n",
        )?;
        let (api, mut agent) = test_configs(root.path());
        agent.skills = SkillsConfig {
            enabled: true,
            project_enabled: true,
            user_dir: user_skills.path().to_path_buf(),
            metadata_budget_bytes: 4_096,
            max_skills: 8,
            max_skill_bytes: 16 * 1024,
            max_resource_bytes: 16 * 1024,
            max_resources: 8,
        };
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (_command_tx, command_rx) = mpsc::channel(16);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator
            .state
            .push_pending_user(1, "review cancellation");

        let request = serde_json::to_value(orchestrator.build_request(1, false)?)?;
        let instructions = request["instructions"].as_str().unwrap_or_default();
        assert!(instructions.contains("Inspect cancellation paths"));
        assert!(!instructions.contains("Private body loaded on demand"));
        let tools = request["tools"].as_array().ok_or("missing skills tools")?;
        let names = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&READ_SKILL_TOOL));
        assert!(names.contains(&LIST_SKILL_RESOURCES_TOOL));
        assert!(names.contains(&READ_SKILL_RESOURCE_TOOL));
        assert!(tools.iter().all(|tool| tool["strict"] == true));
        assert!(!request["parallel_tool_calls"].as_bool().unwrap_or(true));

        orchestrator
            .run_skill_call(
                1,
                FunctionCall {
                    call_id: "skill_call_1".to_owned(),
                    name: READ_SKILL_TOOL.to_owned(),
                    arguments: r#"{"skill_id":"project:review"}"#.to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| format!("skill call failed: {exit:?}"))?;
        assert!(orchestrator.state.history.iter().any(|entry| {
            entry.api_items.iter().any(|item| {
                item["type"] == "function_call_output"
                    && item["call_id"] == "skill_call_1"
                    && item["output"]
                        .as_str()
                        .is_some_and(|output| output.contains("Private body loaded on demand"))
            })
        }));
        Ok(())
    }

    #[tokio::test]
    async fn completed_or_declined_tool_clears_the_confirmation_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (orchestrator, snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let action = ToolAction::ExecuteCommand {
            command: "whoami".to_owned(),
            requires_confirmation: true,
        };
        orchestrator
            .emit(super::OrchestratorEvent::ConfirmationRequested {
                turn_id: 1,
                action_id: 2,
                action: action.clone(),
                command: "whoami".to_owned(),
                command_bytes: 6,
                command_digest: CommandDigest::for_command("whoami"),
                model_requested: true,
                reason: ConfirmationReason::ModelRequested,
                session_trust_available: false,
            })
            .await;
        assert!(snapshot_rx.borrow().modal.is_some());

        orchestrator
            .emit(super::OrchestratorEvent::ToolCompleted {
                conversation_epoch: 1,
                turn_id: 1,
                action_id: 2,
                action: action.clone(),
                outcome: ToolOutcome::Declined { action },
            })
            .await;
        assert!(snapshot_rx.borrow().modal.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn reset_clears_non_persistent_shell_grants() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let command = "cargo check";
        orchestrator.session_shell_permissions.grant_exact(
            command,
            CommandDigest::for_command(command),
            1,
            2,
        );
        orchestrator.publish_shell_permissions("test grant");
        assert_eq!(snapshot_rx.borrow().shell_permissions.grants.len(), 1);

        orchestrator.reset_state().await;

        assert!(
            orchestrator
                .session_shell_permissions
                .snapshot()
                .grants
                .is_empty()
        );
        assert!(snapshot_rx.borrow().shell_permissions.grants.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn whip_retry_note_is_bound_to_its_logical_turn() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.whip_retry_note_pending = Some(7);

        let matching = serde_json::to_value(orchestrator.build_request(7, false)?)?;
        let unrelated = serde_json::to_value(orchestrator.build_request(8, false)?)?;
        let developer_count = |request: &serde_json::Value| {
            request["input"].as_array().map_or(0, |items| {
                items
                    .iter()
                    .filter(|item| item["role"] == "developer")
                    .count()
            })
        };
        assert_eq!(developer_count(&matching), 1);
        assert_eq!(developer_count(&unrelated), 0);
        Ok(())
    }

    #[tokio::test]
    async fn terminal_latch_drains_ready_reset_and_whip_before_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        command_tx.send(super::OrchestratorCommand::Reset).await?;
        let mut boundary = None;
        assert_eq!(
            orchestrator
                .drain_stream_controls(1, 0, &mut boundary)
                .await,
            AwaitedControl::Reset
        );

        // Rebuild after consuming Reset. A ready urgent Whip must be observed
        // by the same terminal-latch drain and leave a retry marker, so a
        // buffered response.completed cannot commit the abandoned attempt.
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        assert_eq!(urgent.whip(1), 1);
        let mut boundary = None;
        assert_eq!(
            orchestrator
                .drain_stream_controls(1, 0, &mut boundary)
                .await,
            AwaitedControl::Continue
        );
        assert_eq!(boundary, Some(0));

        // Once the Completed branch itself has won the biased select, a later
        // Whip is stale. The post-latch busy drain intentionally ignores it.
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        assert_eq!(urgent.whip(1), 1);
        assert_eq!(
            orchestrator.drain_busy_controls(1).await,
            AwaitedControl::Continue
        );
        assert_eq!(orchestrator.penalty_responses_remaining, 0);
        Ok(())
    }

    #[tokio::test]
    async fn pre_mutation_drain_observes_urgent_reset() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        assert_eq!(urgent.reset(), 1);
        assert_eq!(
            orchestrator.drain_busy_controls(1).await,
            AwaitedControl::Reset
        );
        assert!(!root.path().join("must-not-exist.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn parsing_is_preemptible_even_when_worker_is_already_ready()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        assert_eq!(urgent.reset(), 1);
        let result = orchestrator
            .parse_with_controls(1, "ordinary response".to_owned(), &CancellationToken::new())
            .await;
        assert!(matches!(result, Err(super::TurnExit::Reset)));
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_mutating_action_records_its_real_failure_outcome()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let cancel = CancellationToken::new();
        cancel.cancel();
        if let Err(exit) = orchestrator
            .run_action(
                1,
                ToolAction::WriteFile {
                    path: "must-not-exist.txt".to_owned(),
                    content: "content".to_owned(),
                },
                &cancel,
            )
            .await
        {
            return Err(io::Error::other(format!("unexpected turn exit: {exit:?}")).into());
        }

        let entry = orchestrator
            .state
            .history
            .last()
            .ok_or_else(|| io::Error::other("missing tool outcome"))?;
        assert!(matches!(
            &entry.kind,
            super::HistoryKind::ToolResult {
                outcome: super::ToolResultStatus::Failure,
                ..
            }
        ));
        assert!(!root.path().join("must-not-exist.txt").exists());
        Ok(())
    }

    #[tokio::test]
    async fn parallel_reads_start_together_but_finish_in_source_order_with_failure_isolation()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        std::fs::write(root.path().join("ok.txt"), "visible")?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;

        orchestrator
            .run_read_batch(
                11,
                vec![
                    ToolAction::ReadFile {
                        path: "missing.txt".to_owned(),
                    },
                    ToolAction::ReadFile {
                        path: "ok.txt".to_owned(),
                    },
                ],
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;

        let outcomes = orchestrator
            .state
            .history
            .iter()
            .filter_map(|entry| match &entry.kind {
                HistoryKind::ToolResult {
                    action_id, outcome, ..
                } => Some((*action_id, outcome.clone())),
                HistoryKind::User | HistoryKind::Assistant => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            outcomes,
            vec![
                (1, ToolResultStatus::Failure),
                (2, ToolResultStatus::Success)
            ]
        );

        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                super::OrchestratorEvent::ToolStarted { action_id, .. } => {
                    events.push(("start", action_id));
                }
                super::OrchestratorEvent::ToolCompleted { action_id, .. } => {
                    events.push(("complete", action_id));
                }
                _ => {}
            }
        }
        assert_eq!(
            events,
            vec![("start", 1), ("start", 2), ("complete", 1), ("complete", 2)]
        );
        Ok(())
    }

    #[test]
    fn matching_tool_hooks_force_read_batches_back_to_sequential_execution()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let user = tempdir()?;
        let hooks = user.path().join("hooks");
        fs::create_dir_all(&hooks)?;
        let program = user.path().join("reviewed-hook.exe");
        fs::write(&program, "test fixture")?;
        fs::write(
            hooks.join("read-audit.toml"),
            format!(
                "name='Read audit'\nevent='pre_tool_use'\nprogram={:?}\ntool_match=['read_file']\n",
                program.display().to_string()
            ),
        )?;
        let catalog = AutomationCatalog::load_from_for_test(
            dunce::canonicalize(root.path())?,
            Some(user.path().to_path_buf()),
        );
        assert_eq!(catalog.snapshot().hooks.len(), 1);

        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.automation = Arc::new(Mutex::new(catalog));
        let actions = [
            ToolAction::ReadFile {
                path: "one.rs".to_owned(),
            },
            ToolAction::ReadFile {
                path: "two.rs".to_owned(),
            },
        ];
        assert!(!orchestrator.read_batch_has_no_hooks(&actions));
        Ok(())
    }

    #[tokio::test]
    async fn patch_approval_is_scoped_and_malformed_decisions_fail_closed()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (mut orchestrator, snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let review = std::sync::Arc::new(crate::tools::PatchReview::new(
            "src/lib.rs",
            "one\nkeep\nthree\n",
            "ONE\nkeep\nTHREE\n",
        ));
        assert_eq!(review.hunks.len(), 2);

        command_tx
            .send(super::OrchestratorCommand::DecidePatch {
                turn_id: 9,
                action_id: 7,
                decisions: vec![true, true],
            })
            .await?;
        command_tx
            .send(super::OrchestratorCommand::DecidePatch {
                turn_id: 9,
                action_id: 8,
                decisions: vec![true],
            })
            .await?;
        command_tx
            .send(super::OrchestratorCommand::DecidePatch {
                turn_id: 9,
                action_id: 8,
                decisions: vec![true, false],
            })
            .await?;

        let selection = orchestrator
            .await_patch_approval(
                9,
                8,
                std::sync::Arc::clone(&review),
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?
            .ok_or_else(|| io::Error::other("valid partial approval was declined"))?;
        assert_eq!(selection.replacement, "ONE\nkeep\nthree\n");
        assert_eq!(selection.approved_hunks, 1);
        assert!(matches!(
            &snapshot_rx.borrow().modal,
            Some(super::UiModal::PatchApproval {
                turn_id: 9,
                action_id: 8,
                ..
            })
        ));
        let mut saw_fail_closed = false;
        while let Ok(event) = event_rx.try_recv() {
            if let super::OrchestratorEvent::BusyRejected { message, .. } = event
                && message.contains("fail closed")
            {
                saw_fail_closed = true;
            }
        }
        assert!(saw_fail_closed);
        Ok(())
    }

    #[tokio::test]
    async fn auto_approval_accepts_selected_classes_but_never_model_or_forced_shell_prompts()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        orchestrator.state.auto_approval.shell = true;
        orchestrator.state.auto_approval.workspace_changes = true;

        let review = Arc::new(crate::tools::PatchReview::new(
            "src/lib.rs",
            "old\n",
            "new\n",
        ));
        let selection = orchestrator
            .await_patch_approval(1, 1, review, &CancellationToken::new())
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?
            .ok_or_else(|| io::Error::other("auto-approved patch was declined"))?;
        assert_eq!(selection.approved_hunks, 1);

        let action = ToolAction::ExecuteCommand {
            command: "git status".to_owned(),
            requires_confirmation: false,
        };
        let binding = ApprovalBinding {
            epoch: orchestrator.conversation_epoch,
            turn_id: 2,
            action_id: 2,
            nonce: ApprovalNonce::new([7; 16]),
            command_digest: CommandDigest::for_command("git status"),
        };
        let approval = orchestrator
            .await_confirmation(
                2,
                2,
                &action,
                binding,
                ConfirmationDecision::RequiresUserConfirmation {
                    reason: ConfirmationReason::PolicyRequired,
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;
        assert!(approval.is_some());

        command_tx
            .send(super::OrchestratorCommand::Confirm {
                turn_id: 3,
                action_id: 3,
                decision: ShellApprovalDecision::Decline,
            })
            .await?;
        let model_binding = ApprovalBinding {
            epoch: orchestrator.conversation_epoch,
            turn_id: 3,
            action_id: 3,
            nonce: ApprovalNonce::new([8; 16]),
            command_digest: CommandDigest::for_command("git status"),
        };
        let approval = orchestrator
            .await_confirmation(
                3,
                3,
                &action,
                model_binding,
                ConfirmationDecision::RequiresUserConfirmation {
                    reason: ConfirmationReason::ModelRequested,
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;
        assert!(approval.is_none());
        let mut saw_model_confirmation = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(
                event,
                super::OrchestratorEvent::ConfirmationRequested {
                    reason: ConfirmationReason::ModelRequested,
                    ..
                }
            ) {
                saw_model_confirmation = true;
            }
        }
        assert!(saw_model_confirmation);

        command_tx
            .send(super::OrchestratorCommand::Confirm {
                turn_id: 4,
                action_id: 4,
                decision: ShellApprovalDecision::Decline,
            })
            .await?;
        let forced_binding = ApprovalBinding {
            epoch: orchestrator.conversation_epoch,
            turn_id: 4,
            action_id: 4,
            nonce: ApprovalNonce::new([9; 16]),
            command_digest: CommandDigest::for_command("git status"),
        };
        let approval = orchestrator
            .await_confirmation(
                4,
                4,
                &action,
                forced_binding,
                ConfirmationDecision::RequiresUserConfirmation {
                    reason: ConfirmationReason::ForcedRule("test hard rule"),
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;
        assert!(approval.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn write_file_requires_hunk_review_and_commits_only_the_selected_content()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        std::fs::write(root.path().join("notes.txt"), "old\nkeep\nlast\n")?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        command_tx
            .send(super::OrchestratorCommand::DecidePatch {
                turn_id: 5,
                action_id: 1,
                decisions: vec![true, false],
            })
            .await?;

        orchestrator
            .run_action(
                5,
                ToolAction::WriteFile {
                    path: "notes.txt".to_owned(),
                    content: "NEW\nkeep\nLAST\n".to_owned(),
                },
                &CancellationToken::new(),
            )
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;

        assert_eq!(
            std::fs::read_to_string(root.path().join("notes.txt"))?,
            "NEW\nkeep\nlast\n"
        );
        assert!(matches!(
            orchestrator.state.history.last().map(|entry| &entry.kind),
            Some(super::HistoryKind::ToolResult {
                outcome: super::ToolResultStatus::Success,
                ..
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn rejecting_all_patch_hunks_never_returns_an_executable_selection()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let (api, agent) = test_configs(root.path());
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (command_tx, command_rx) = mpsc::channel(8);
        let (mut orchestrator, _snapshot_rx, _urgent) =
            Orchestrator::with_runtime(api, agent, event_tx, command_rx)?;
        let review = std::sync::Arc::new(crate::tools::PatchReview::new(
            "src/lib.rs",
            "old\n",
            "new\n",
        ));
        command_tx
            .send(super::OrchestratorCommand::DecidePatch {
                turn_id: 3,
                action_id: 4,
                decisions: vec![false],
            })
            .await?;
        let selection = orchestrator
            .await_patch_approval(3, 4, review, &CancellationToken::new())
            .await
            .map_err(|exit| io::Error::other(format!("unexpected turn exit: {exit:?}")))?;
        assert!(selection.is_none());
        Ok(())
    }
}
