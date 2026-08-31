use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
    panic::PanicHookInfo,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use crossterm::{
    cursor::Show,
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, SetTitle, disable_raw_mode, enable_raw_mode,
    },
};
use ratatui::{Terminal, backend::CrosstermBackend, layout::Rect};
use tokio::sync::mpsc;

use crate::{
    agent::{
        AutoApprovalPolicy, CheckpointSummary, FollowUpSnapshot, InstructionSetSnapshot,
        PlanReview, ReviewCatalogSnapshot, SessionId, SessionSummary, ShellPermissionSnapshot,
        SideChatSnapshot, SkillCatalogSnapshot, SubagentFileReview, SubagentFleetSnapshot,
        WhipKind, WorkModes,
        automation::AutomationSnapshot,
        orchestrator::{
            McpToolCall, Orchestrator, OrchestratorCommand, OrchestratorEvent, RetrySnapshot,
            UiModal, UiSnapshot, UrgentControlHandle, WhipTelemetry,
        },
        phase::AgentPhase,
        state::{ActionId, ContinuationId, HistoryEntry, HistoryKind, HistoryStatus, TurnId},
    },
    api::ReasoningEffort,
    attachments::AttachmentDraft,
    code_index::{CodeIndexHit, CodeIndexSnapshot},
    config::{AppConfig, ContextMode, UiLanguage},
    error::AppError,
    github::GitHubSnapshot,
    lsp::{LspDiagnostic, LspServerSnapshot},
    mcp::{McpOAuthPrompt, McpServerSnapshot},
    parser::tool_action::{ToolAction, ToolOutcome},
    plugins::PluginSnapshot,
    privacy::PrivacySnapshot,
    terminal::{TerminalFleetSnapshot, start_terminal_runtime},
    tools::{CommandDigest, ConfirmationReason, PatchReview},
    usage::UsageSnapshot,
};

use super::{
    agents::AgentUiState,
    approval_center::ApprovalCenterUiState,
    automation::AutomationUiState,
    code_index::CodeIndexUiState,
    confirm::{ConfirmationUiState, ContinuationUiState},
    eta::EtaTracker,
    followups::FollowUpUiState,
    github::GitHubUiState,
    i18n::{Text, notice_text, text},
    input,
    instructions::InstructionsUiState,
    language::LanguageUiState,
    lsp::LspUiState,
    mascot::MascotState,
    mcp::McpUiState,
    modes::{ModesUiState, PlanApprovalUiState},
    notifications::{NotificationCenter, NotificationKind, NotificationUiState},
    palette::PaletteUiState,
    patch_review::PatchReviewUiState,
    permissions::PermissionUiState,
    plugins::PluginUiState,
    privacy::PrivacyUiState,
    render,
    review::ReviewUiState,
    rewind::RewindUiState,
    runtime::RuntimeUiState,
    sessions::SessionUiState,
    shell::ShellUiState,
    side_chat::SideChatUiState,
    skills::SkillsUiState,
    terminal::{TerminalUiState, terminal_control_error_text, terminal_notice_text},
    usage::UsageUiState,
    whip::WhipController,
};

const EVENT_CHANNEL_CAPACITY: usize = 256;
const COMMAND_CHANNEL_CAPACITY: usize = 64;
const ORCHESTRATOR_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

type PanicHook = Box<dyn Fn(&PanicHookInfo<'_>) + Send + Sync + 'static>;

#[derive(Debug, Clone)]
pub struct PendingConfirmation {
    pub turn_id: TurnId,
    pub action_id: ActionId,
    pub action: ToolAction,
    pub command: String,
    pub command_bytes: usize,
    pub command_digest: CommandDigest,
    pub model_requested: bool,
    pub reason: ConfirmationReason,
    pub session_trust_available: bool,
}

#[derive(Debug, Clone)]
pub struct PendingMcpConfirmation {
    pub turn_id: TurnId,
    pub action_id: ActionId,
    pub call: Arc<McpToolCall>,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingContinuation {
    pub turn_id: TurnId,
    pub continuation_id: ContinuationId,
    pub completed_iterations: u32,
    pub max_iterations: u32,
}

#[derive(Debug, Clone)]
pub struct PendingPatchReview {
    pub turn_id: TurnId,
    pub action_id: ActionId,
    pub review: Arc<PatchReview>,
}

#[derive(Debug, Clone)]
pub struct PendingSubagentReview {
    pub review: Arc<SubagentFileReview>,
}

#[derive(Debug, Clone)]
pub struct PendingPlanReview {
    pub review: Arc<PlanReview>,
}

#[derive(Debug)]
struct NotificationSeed {
    causal_key: String,
    title: String,
    body: String,
}

fn notification_for_modal(modal: Option<&UiModal>) -> Option<NotificationSeed> {
    match modal? {
        UiModal::PlanApproval { review } => Some(NotificationSeed {
            causal_key: format!("plan:{}:{}", review.turn_id, review.review_id),
            title: text(Text::ReadOnlyPlanReview).to_owned(),
            body: text(Text::PlanModeDescription).to_owned(),
        }),
        UiModal::Confirmation {
            turn_id,
            action_id,
            action,
            ..
        } => Some(NotificationSeed {
            causal_key: format!("tool-approval:{turn_id}:{action_id}"),
            title: text(Text::ConfirmShellCommand).to_owned(),
            body: format!(
                "{}: {}",
                text(Text::WaitingCommandApproval),
                action.tool_name()
            ),
        }),
        UiModal::McpConfirmation {
            turn_id,
            action_id,
            call,
            reason,
        } => Some(NotificationSeed {
            causal_key: format!("mcp-approval:{turn_id}:{action_id}"),
            title: text(Text::ApproveMcpTool).to_owned(),
            body: format!("{} / {}: {reason}", call.server, call.tool),
        }),
        UiModal::PatchApproval {
            turn_id,
            action_id,
            review,
        } => Some(NotificationSeed {
            causal_key: format!("patch-approval:{turn_id}:{action_id}"),
            title: text(Text::ReviewPatchHunks).to_owned(),
            body: format!("{}: {}", text(Text::ReviewPatchHunks), review.hunks.len()),
        }),
        UiModal::Continuation {
            turn_id,
            continuation_id,
            completed_iterations,
            max_iterations,
        } => Some(NotificationSeed {
            causal_key: format!("continuation:{turn_id}:{continuation_id}"),
            title: text(Text::ToolIterationLimit).to_owned(),
            body: format!(
                "{}: {completed_iterations}/{max_iterations}",
                text(Text::ContinueWindowHelp)
            ),
        }),
        UiModal::SubagentPatchApproval { review } => Some(NotificationSeed {
            causal_key: format!(
                "subagent-review:{}:{}:{}",
                review.agent_id.get(),
                review.agent_revision,
                review.change_digest
            ),
            title: text(Text::ReviewChanges).to_owned(),
            body: format!("{}: {}", text(Text::ReviewLabel), review.path),
        }),
    }
}

pub struct AppState {
    pub phase: AgentPhase,
    pub active_turn_id: Option<TurnId>,
    pub paused_turn_id: Option<TurnId>,
    pub history: Arc<[HistoryEntry]>,
    pub checkpoints: Arc<[CheckpointSummary]>,
    pub sessions: Arc<[SessionSummary]>,
    pub current_session_id: Option<SessionId>,
    pub session_ui: SessionUiState,
    pub workspace_root: PathBuf,
    pub workspace_files: Arc<[String]>,
    pub shell_ui: ShellUiState,
    pub terminal: TerminalFleetSnapshot,
    pub terminal_ui: TerminalUiState,
    pub subagents: SubagentFleetSnapshot,
    pub agents_ui: AgentUiState,
    pub palette_ui: PaletteUiState,
    pub deployment: String,
    pub provider: String,
    pub deployment_choices: Arc<[String]>,
    pub reasoning_effort: ReasoningEffort,
    pub work_modes: WorkModes,
    pub auto_approval: AutoApprovalPolicy,
    pub instructions: InstructionSetSnapshot,
    pub skills: SkillCatalogSnapshot,
    pub automation: AutomationSnapshot,
    pub plugins: PluginSnapshot,
    pub runtime_ui: RuntimeUiState,
    pub mcp_ui: McpUiState,
    pub lsp_ui: LspUiState,
    pub code_index_ui: CodeIndexUiState,
    pub privacy_ui: PrivacyUiState,
    pub permission_ui: PermissionUiState,
    pub usage_ui: UsageUiState,
    pub side_chat_ui: SideChatUiState,
    pub follow_up_ui: FollowUpUiState,
    pub modes_ui: ModesUiState,
    pub instructions_ui: InstructionsUiState,
    pub language: UiLanguage,
    pub language_ui: LanguageUiState,
    pub skills_ui: SkillsUiState,
    pub automation_ui: AutomationUiState,
    pub plugin_ui: PluginUiState,
    pub approval_center_ui: ApprovalCenterUiState,
    pub live_thinking: String,
    pub live_assistant: String,
    pub interrupted_draft: String,
    pub show_thinking: bool,
    pub show_tool_activity: bool,
    pub input_buffer: String,
    pub pending_attachments: Vec<AttachmentDraft>,
    /// UTF-8 byte offset at a grapheme boundary.
    pub input_cursor: usize,
    pub pending_confirmation: Option<PendingConfirmation>,
    pub pending_mcp_confirmation: Option<PendingMcpConfirmation>,
    pub confirmation_ui: ConfirmationUiState,
    pub pending_continuation: Option<PendingContinuation>,
    pub continuation_ui: ContinuationUiState,
    pub pending_patch_review: Option<PendingPatchReview>,
    pub pending_plan_review: Option<PendingPlanReview>,
    pub pending_subagent_review: Option<PendingSubagentReview>,
    pub plan_approval_ui: PlanApprovalUiState,
    pub patch_review_ui: PatchReviewUiState,
    pub rewind_ui: RewindUiState,
    pub status_message: Option<String>,
    pub tokens_input: u64,
    pub tokens_output: u64,
    pub tokens_total: u64,
    pub usage: UsageSnapshot,
    pub side_chat: SideChatSnapshot,
    pub follow_ups: FollowUpSnapshot,
    pub reviews: ReviewCatalogSnapshot,
    pub review_ui: ReviewUiState,
    pub notifications: NotificationCenter,
    pub notification_ui: NotificationUiState,
    pub github: GitHubSnapshot,
    pub github_ui: GitHubUiState,
    pub mascot: MascotState,
    pub eta: EtaTracker,
    pub should_quit: bool,
    pub whip: WhipController,
    pub whip_hitbox: Option<Rect>,
    pub scroll_offset: u16,
    pub conversation_epoch: u64,
    pub phase_revision: u64,
    pub history_revision: u64,
    pub phase_started: Instant,
    pub connected: bool,
    pub connection_status: String,
    pub context_mode: &'static str,
    pub context_budget: u32,
    pub max_context_budget: u32,
    pub retry: Option<RetrySnapshot>,
    pub mcp_servers: Arc<[McpServerSnapshot]>,
    pub mcp_oauth_prompt: Option<McpOAuthPrompt>,
    pub lsp_servers: Arc<[LspServerSnapshot]>,
    pub lsp_diagnostics: Arc<[LspDiagnostic]>,
    pub code_index: CodeIndexSnapshot,
    pub code_index_hits: Arc<[CodeIndexHit]>,
    pub privacy: PrivacySnapshot,
    pub shell_permissions: ShellPermissionSnapshot,
    /// Tool metadata is opportunistic UI detail; authoritative state remains in snapshots.
    pub tool_actions: Arc<BTreeMap<ActionId, ToolAction>>,
    pub running_tools: BTreeSet<ActionId>,
    pub mcp_calls: Arc<BTreeMap<ActionId, Arc<McpToolCall>>>,
    pub expanded_tools: BTreeSet<ActionId>,
    pub selected_tool: Option<ActionId>,
    pub confirmation_scroll: usize,
    pub confirmation_max_scroll: usize,
    pub confirmation_view_ready: bool,
    pub confirmation_suffix_viewed: bool,
    /// Set only by `End`; the renderer consumes it after drawing the true last row.
    pub confirmation_end_requested: bool,
    pub whip_telemetry: WhipTelemetry,
    pub whip_enabled: bool,
    pub whip_hotkey: char,
    notification_baseline_initialized: bool,
    active_attention_key: Option<String>,
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            phase: AgentPhase::Idle,
            active_turn_id: None,
            paused_turn_id: None,
            history: Arc::from([]),
            checkpoints: Arc::from([]),
            sessions: Arc::from([]),
            current_session_id: None,
            session_ui: SessionUiState::new(),
            workspace_root: PathBuf::new(),
            workspace_files: Arc::from([]),
            shell_ui: ShellUiState::new(),
            terminal: TerminalFleetSnapshot::default(),
            terminal_ui: TerminalUiState::new(),
            subagents: SubagentFleetSnapshot::default(),
            agents_ui: AgentUiState::new(),
            palette_ui: PaletteUiState::new(),
            deployment: String::new(),
            provider: "unknown".to_owned(),
            deployment_choices: Arc::from([]),
            reasoning_effort: ReasoningEffort::Medium,
            work_modes: WorkModes::default(),
            auto_approval: AutoApprovalPolicy::default(),
            instructions: InstructionSetSnapshot::default(),
            skills: SkillCatalogSnapshot::default(),
            automation: AutomationSnapshot::default(),
            plugins: PluginSnapshot::default(),
            runtime_ui: RuntimeUiState::new(),
            mcp_ui: McpUiState::new(),
            lsp_ui: LspUiState::new(),
            code_index_ui: CodeIndexUiState::new(),
            privacy_ui: PrivacyUiState::new(),
            permission_ui: PermissionUiState::new(),
            usage_ui: UsageUiState::new(),
            side_chat_ui: SideChatUiState::new(),
            follow_up_ui: FollowUpUiState::new(),
            modes_ui: ModesUiState::new(),
            instructions_ui: InstructionsUiState::new(),
            language: UiLanguage::English,
            language_ui: LanguageUiState::new(),
            skills_ui: SkillsUiState::new(),
            automation_ui: AutomationUiState::new(),
            plugin_ui: PluginUiState::new(),
            approval_center_ui: ApprovalCenterUiState::new(),
            live_thinking: String::new(),
            live_assistant: String::new(),
            interrupted_draft: String::new(),
            show_thinking: true,
            show_tool_activity: true,
            input_buffer: String::new(),
            pending_attachments: Vec::new(),
            input_cursor: 0,
            pending_confirmation: None,
            pending_mcp_confirmation: None,
            confirmation_ui: ConfirmationUiState::new(),
            pending_continuation: None,
            continuation_ui: ContinuationUiState::new(),
            pending_patch_review: None,
            pending_plan_review: None,
            pending_subagent_review: None,
            plan_approval_ui: PlanApprovalUiState::new(),
            patch_review_ui: PatchReviewUiState::new(),
            rewind_ui: RewindUiState::new(),
            status_message: None,
            tokens_input: 0,
            tokens_output: 0,
            tokens_total: 0,
            usage: UsageSnapshot::default(),
            side_chat: SideChatSnapshot::default(),
            follow_ups: FollowUpSnapshot::default(),
            reviews: ReviewCatalogSnapshot::default(),
            review_ui: ReviewUiState::new(),
            notifications: NotificationCenter::new(),
            notification_ui: NotificationUiState::new(),
            github: GitHubSnapshot::default(),
            github_ui: GitHubUiState::new(),
            mascot: MascotState::new(true),
            eta: EtaTracker::new(),
            should_quit: false,
            whip: WhipController::new(),
            whip_hitbox: None,
            scroll_offset: 0,
            conversation_epoch: 1,
            phase_revision: 0,
            history_revision: 0,
            phase_started: Instant::now(),
            connected: true,
            connection_status: "idle".to_owned(),
            context_mode: "-",
            context_budget: 0,
            max_context_budget: crate::config::MAX_CONTEXT_BUDGET,
            retry: None,
            mcp_servers: Arc::from([]),
            mcp_oauth_prompt: None,
            lsp_servers: Arc::from([]),
            lsp_diagnostics: Arc::from([]),
            code_index: CodeIndexSnapshot::new(false),
            code_index_hits: Arc::from([]),
            privacy: PrivacySnapshot::default(),
            shell_permissions: ShellPermissionSnapshot::default(),
            tool_actions: Arc::new(BTreeMap::new()),
            running_tools: BTreeSet::new(),
            mcp_calls: Arc::new(BTreeMap::new()),
            expanded_tools: BTreeSet::new(),
            selected_tool: None,
            confirmation_scroll: 0,
            confirmation_max_scroll: 0,
            confirmation_view_ready: false,
            confirmation_suffix_viewed: false,
            confirmation_end_requested: false,
            whip_telemetry: WhipTelemetry::default(),
            whip_enabled: true,
            whip_hotkey: 'w',
            notification_baseline_initialized: false,
            active_attention_key: None,
        }
    }

    #[must_use]
    pub fn has_blocking_modal(&self) -> bool {
        self.pending_confirmation.is_some()
            || self.pending_mcp_confirmation.is_some()
            || self.pending_continuation.is_some()
            || self.pending_patch_review.is_some()
            || self.pending_plan_review.is_some()
            || self.pending_subagent_review.is_some()
            || self.session_ui.is_open()
            || self.rewind_ui.is_open()
            || self.shell_ui.menu_is_open()
            || self.shell_ui.tool_menu_is_open()
            || self.palette_ui.is_open()
            || self.runtime_ui.is_open()
            || self.mcp_ui.is_open()
            || self.lsp_ui.is_open()
            || self.code_index_ui.is_open()
            || self.privacy_ui.is_open()
            || self.permission_ui.is_open()
            || self.usage_ui.is_open()
            || self.side_chat_ui.is_open()
            || self.follow_up_ui.is_open()
            || self.review_ui.is_open()
            || self.notification_ui.is_open()
            || self.github_ui.is_open()
            || self.modes_ui.is_open()
            || self.instructions_ui.is_open()
            || self.language_ui.is_open()
            || self.skills_ui.is_open()
            || self.automation_ui.is_open()
            || self.plugin_ui.is_open()
            || self.approval_center_ui.is_open()
            || !matches!(self.agents_ui.editor(), super::agents::AgentEditor::Closed)
            || self
                .subagents
                .agents
                .iter()
                .any(|agent| agent.pending_command.is_some())
    }

    #[must_use]
    pub fn pending_confirmation_ids(&self) -> Option<(TurnId, ActionId)> {
        self.pending_confirmation
            .as_ref()
            .map(|pending| (pending.turn_id, pending.action_id))
            .or_else(|| {
                self.pending_mcp_confirmation
                    .as_ref()
                    .map(|pending| (pending.turn_id, pending.action_id))
            })
    }

    #[must_use]
    pub fn can_whip(&self) -> bool {
        self.whip_enabled
            && matches!(&self.phase, AgentPhase::Requesting | AgentPhase::Streaming)
            && self.active_turn_id.is_some()
            && !self.has_blocking_modal()
    }

    pub fn reset_view(&mut self) {
        self.phase = AgentPhase::Idle;
        self.active_turn_id = None;
        self.paused_turn_id = None;
        self.history = Arc::from([]);
        self.live_thinking.clear();
        self.live_assistant.clear();
        self.interrupted_draft.clear();
        self.pending_attachments.clear();
        self.pending_confirmation = None;
        self.pending_mcp_confirmation = None;
        self.confirmation_ui.reset();
        self.pending_continuation = None;
        self.continuation_ui.reset();
        self.pending_patch_review = None;
        self.pending_plan_review = None;
        self.pending_subagent_review = None;
        self.patch_review_ui.close();
        self.session_ui.close();
        self.palette_ui.close();
        self.runtime_ui.close();
        self.mcp_ui.close();
        self.lsp_ui.close();
        self.code_index_ui.close();
        self.privacy_ui.close();
        self.permission_ui.close();
        self.usage_ui.close();
        self.side_chat_ui.close();
        self.follow_up_ui.close();
        self.review_ui.close();
        self.notification_ui.close();
        self.github_ui.close();
        self.modes_ui.close();
        self.instructions_ui.close();
        self.language_ui.close();
        self.skills_ui.close();
        self.automation_ui.close();
        self.plugin_ui.close();
        self.approval_center_ui.close();
        self.plan_approval_ui.sync(None);
        self.rewind_ui.close();
        self.status_message = Some(text(Text::ConversationReset).to_owned());
        self.scroll_offset = 0;
        self.tool_actions = Arc::new(BTreeMap::new());
        self.running_tools.clear();
        self.mcp_calls = Arc::new(BTreeMap::new());
        self.expanded_tools.clear();
        self.selected_tool = None;
        self.confirmation_scroll = 0;
        self.confirmation_max_scroll = 0;
        self.confirmation_view_ready = false;
        self.confirmation_suffix_viewed = false;
        self.confirmation_end_requested = false;
        self.phase_started = Instant::now();
        self.eta.reset_turn();
        self.retry = None;
        self.connection_status = "idle".to_owned();
    }

    pub fn apply_snapshot(&mut self, snapshot: &UiSnapshot) {
        if snapshot.conversation_epoch < self.conversation_epoch {
            return;
        }
        if snapshot.conversation_epoch == self.conversation_epoch
            && (snapshot.phase_revision < self.phase_revision
                || snapshot.history_revision < self.history_revision)
        {
            return;
        }
        let previous_epoch = self.conversation_epoch;
        let previous_phase_revision = self.phase_revision;
        let previous_phase = self.phase.clone();
        let previous_turn = self.active_turn_id;
        let previous_attention_key = self.active_attention_key.clone();
        if snapshot.conversation_epoch > self.conversation_epoch {
            self.reset_view();
            self.tokens_input = 0;
            self.tokens_output = 0;
            self.tokens_total = 0;
            self.usage = UsageSnapshot::default();
            self.side_chat = SideChatSnapshot::default();
            self.follow_ups = FollowUpSnapshot::default();
            self.reviews = ReviewCatalogSnapshot::default();
            self.whip = WhipController::new();
        }
        if self.phase != snapshot.phase || self.phase_revision != snapshot.phase_revision {
            self.phase_started = Instant::now();
        }
        let previous_confirmation = self.pending_confirmation_ids();
        let previous_continuation = self
            .pending_continuation
            .map(|pending| (pending.turn_id, pending.continuation_id));
        let previous_patch_review = self
            .pending_patch_review
            .as_ref()
            .map(|pending| (pending.turn_id, pending.action_id));
        let previous_subagent_review = self.pending_subagent_review.as_ref().map(|pending| {
            (
                pending.review.agent_id,
                pending.review.agent_revision,
                pending.review.path.clone(),
                pending.review.change_digest.clone(),
            )
        });
        let previous_subagent_command = self.subagents.agents.iter().find_map(|agent| {
            agent
                .pending_command
                .as_ref()
                .map(|pending| (agent.id, agent.revision, pending.action_id))
        });
        self.phase = snapshot.phase.clone();
        self.active_turn_id = snapshot.active_turn_id;
        self.paused_turn_id = snapshot.paused_turn_id;
        if previous_phase.is_busy() && !snapshot.phase.is_busy() {
            let now = Instant::now();
            if matches!(snapshot.phase, AgentPhase::Idle)
                && snapshot.paused_turn_id.is_none()
                && snapshot.retry.is_none()
            {
                if let Some(turn_id) = previous_turn {
                    self.eta.complete(turn_id, now);
                }
            } else {
                self.eta.suspend(previous_turn, now);
            }
        }
        self.history = Arc::clone(&snapshot.history);
        self.checkpoints = Arc::clone(&snapshot.checkpoints);
        self.sessions = Arc::clone(&snapshot.sessions);
        self.session_ui.sync(&self.sessions);
        self.current_session_id
            .clone_from(&snapshot.current_session_id);
        self.deployment.clone_from(&snapshot.deployment);
        self.reasoning_effort = snapshot.reasoning_effort;
        if snapshot.context_budget > 0 {
            self.context_budget = snapshot.context_budget;
        }
        self.max_context_budget = snapshot.max_context_budget;
        self.github.clone_from(&snapshot.github);
        self.github_ui.sync(&self.github);
        self.work_modes.clone_from(&snapshot.work_modes);
        self.refresh_eta_context();
        self.auto_approval = snapshot.auto_approval;
        self.instructions.clone_from(&snapshot.instructions);
        self.instructions_ui.sync(&self.instructions);
        self.skills.clone_from(&snapshot.skills);
        self.skills_ui.sync(&self.skills);
        self.automation.clone_from(&snapshot.automation);
        self.automation_ui.sync(&self.automation);
        self.plugins.clone_from(&snapshot.plugins);
        self.plugin_ui.sync(&self.plugins);
        self.rewind_ui.set_total(self.checkpoints.len());
        self.tool_actions = Arc::clone(&snapshot.tool_actions);
        if !snapshot.phase.is_busy() {
            self.running_tools.clear();
        }
        self.mcp_calls = Arc::clone(&snapshot.mcp_calls);
        self.live_thinking.clone_from(&snapshot.thinking);
        self.live_assistant = render::strip_service_blocks(&snapshot.assistant);
        if self.live_assistant_is_in_history_for(snapshot.active_turn_id) {
            self.live_assistant.clear();
        }
        self.interrupted_draft
            .clone_from(&snapshot.interrupted_draft);
        self.conversation_epoch = snapshot.conversation_epoch;
        self.phase_revision = snapshot.phase_revision;
        self.history_revision = snapshot.history_revision;
        self.whip_telemetry = snapshot.whip.clone();
        self.connection_status
            .clone_from(&snapshot.connection_status);
        self.retry.clone_from(&snapshot.retry);
        self.mcp_servers = Arc::clone(&snapshot.mcp_servers);
        self.mcp_ui.sync(self.mcp_servers.as_ref());
        self.mcp_oauth_prompt.clone_from(&snapshot.mcp_oauth_prompt);
        self.lsp_servers = Arc::clone(&snapshot.lsp_servers);
        self.lsp_diagnostics = Arc::clone(&snapshot.lsp_diagnostics);
        self.lsp_ui
            .sync(self.lsp_servers.as_ref(), self.lsp_diagnostics.as_ref());
        self.code_index.clone_from(&snapshot.code_index);
        self.code_index_hits = Arc::clone(&snapshot.code_index_hits);
        self.code_index_ui.set_results(self.code_index_hits.len());
        self.privacy.clone_from(&snapshot.privacy);
        self.privacy_ui.sync(&self.privacy);
        self.shell_permissions
            .clone_from(&snapshot.shell_permissions);
        self.permission_ui.sync(&self.shell_permissions);
        let previous_side_revision = self.side_chat.revision;
        let previous_side_count = self.side_chat.exchanges.len();
        self.side_chat.clone_from(&snapshot.side_chat);
        let previous_follow_up_count = self.follow_ups.items.len();
        self.follow_ups.clone_from(&snapshot.follow_ups);
        self.follow_up_ui.sync(&self.follow_ups);
        self.reviews.clone_from(&snapshot.reviews);
        self.review_ui.sync(&self.reviews);
        if self.follow_ups.items.len() > previous_follow_up_count {
            self.follow_up_ui
                .select(self.follow_ups.items.len().saturating_sub(1));
            if self.follow_up_ui.is_open() {
                self.follow_up_ui.browse();
            }
        }
        self.side_chat_ui.sync(&self.side_chat);
        if self.side_chat.exchanges.len() > previous_side_count {
            self.side_chat_ui
                .select_history(self.side_chat.exchanges.len().saturating_sub(1));
        }
        if self.side_chat_ui.is_open()
            && self.side_chat.revision > previous_side_revision
            && self.side_chat.latest().is_some()
        {
            self.side_chat_ui.show_transcript();
        }
        self.subagents = snapshot.subagents.clone();
        self.agents_ui.sync(&self.subagents);
        let current_subagent_command = self.subagents.agents.iter().find_map(|agent| {
            agent
                .pending_command
                .as_ref()
                .map(|pending| (agent.id, agent.revision, pending.action_id))
        });
        if current_subagent_command != previous_subagent_command {
            self.agents_ui.hide_command_dialog();
        }
        self.status_message = Some(if snapshot.notice.is_none() {
            localized_phase_status(&snapshot.phase)
        } else {
            notice_text(&snapshot.notice)
        });
        if let Some(usage) = &snapshot.usage {
            self.usage.clone_from(usage);
            self.tokens_input = usage.usage.input_tokens;
            self.tokens_output = usage.usage.output_tokens;
            self.tokens_total = usage.usage.total_tokens;
        } else {
            self.usage = UsageSnapshot::default();
            self.tokens_input = 0;
            self.tokens_output = 0;
            self.tokens_total = 0;
        }
        self.usage_ui.sync(&self.usage);
        self.pending_confirmation = match &snapshot.modal {
            Some(UiModal::Confirmation {
                turn_id,
                action_id,
                action,
                command,
                command_bytes,
                command_digest,
                model_requested,
                reason,
                session_trust_available,
            }) => Some(PendingConfirmation {
                turn_id: *turn_id,
                action_id: *action_id,
                action: action.clone(),
                command: command.clone(),
                command_bytes: *command_bytes,
                command_digest: *command_digest,
                model_requested: *model_requested,
                reason: *reason,
                session_trust_available: *session_trust_available,
            }),
            _ => None,
        };
        self.pending_mcp_confirmation = match &snapshot.modal {
            Some(UiModal::McpConfirmation {
                turn_id,
                action_id,
                call,
                reason,
            }) => Some(PendingMcpConfirmation {
                turn_id: *turn_id,
                action_id: *action_id,
                call: Arc::clone(call),
                reason: reason.clone(),
            }),
            _ => None,
        };
        self.pending_plan_review = match &snapshot.modal {
            Some(UiModal::PlanApproval { review }) => Some(PendingPlanReview {
                review: Arc::clone(review),
            }),
            _ => None,
        };
        self.plan_approval_ui.sync(
            self.pending_plan_review
                .as_ref()
                .map(|pending| &pending.review),
        );
        let current_confirmation = self.pending_confirmation_ids();
        if current_confirmation != previous_confirmation {
            self.confirmation_ui.reset();
            self.confirmation_scroll = 0;
            self.confirmation_max_scroll = 0;
            self.confirmation_view_ready = false;
            self.confirmation_suffix_viewed = false;
            self.confirmation_end_requested = false;
        }
        self.pending_continuation = match &snapshot.modal {
            Some(UiModal::Continuation {
                turn_id,
                continuation_id,
                completed_iterations,
                max_iterations,
            }) => Some(PendingContinuation {
                turn_id: *turn_id,
                continuation_id: *continuation_id,
                completed_iterations: *completed_iterations,
                max_iterations: *max_iterations,
            }),
            _ => None,
        };
        let current_continuation = self
            .pending_continuation
            .map(|pending| (pending.turn_id, pending.continuation_id));
        if current_continuation != previous_continuation {
            self.continuation_ui.reset();
        }
        self.pending_patch_review = match &snapshot.modal {
            Some(UiModal::PatchApproval {
                turn_id,
                action_id,
                review,
            }) => Some(PendingPatchReview {
                turn_id: *turn_id,
                action_id: *action_id,
                review: Arc::clone(review),
            }),
            _ => None,
        };
        let current_patch_review = self
            .pending_patch_review
            .as_ref()
            .map(|pending| (pending.turn_id, pending.action_id));
        if current_patch_review != previous_patch_review {
            if let Some(pending) = &self.pending_patch_review {
                self.patch_review_ui.open(pending.review.hunks.len());
            } else {
                self.patch_review_ui.close();
            }
        }
        self.pending_subagent_review = match &snapshot.modal {
            Some(UiModal::SubagentPatchApproval { review }) => Some(PendingSubagentReview {
                review: Arc::clone(review),
            }),
            _ => None,
        };
        let current_subagent_review = self.pending_subagent_review.as_ref().map(|pending| {
            (
                pending.review.agent_id,
                pending.review.agent_revision,
                pending.review.path.clone(),
                pending.review.change_digest.clone(),
            )
        });
        if current_subagent_review != previous_subagent_review {
            self.agents_ui.hide_binary_dialog();
            if let Some(review) = self
                .pending_subagent_review
                .as_ref()
                .and_then(|pending| pending.review.review.as_ref())
            {
                self.patch_review_ui.open(review.hunks.len());
            } else if self.pending_patch_review.is_none() {
                self.patch_review_ui.close();
            }
        }
        self.observe_snapshot_notifications(
            snapshot,
            previous_epoch,
            previous_phase_revision,
            &previous_phase,
            previous_turn,
            previous_attention_key,
        );
    }

    fn observe_snapshot_notifications(
        &mut self,
        snapshot: &UiSnapshot,
        previous_epoch: u64,
        previous_phase_revision: u64,
        previous_phase: &AgentPhase,
        previous_turn: Option<TurnId>,
        previous_attention_key: Option<String>,
    ) {
        let current_attention = notification_for_modal(snapshot.modal.as_ref());
        let current_attention_key = current_attention
            .as_ref()
            .map(|notification| notification.causal_key.clone());
        if !self.notification_baseline_initialized {
            self.active_attention_key = current_attention_key;
            self.notification_baseline_initialized = true;
            return;
        }

        if previous_attention_key != current_attention_key {
            if let Some(previous_key) = previous_attention_key.as_deref() {
                self.notifications.resolve(previous_key);
            }
            if let Some(notification) = current_attention {
                self.notifications.push_unique(
                    notification.causal_key,
                    NotificationKind::NeedsAction,
                    notification.title,
                    notification.body,
                );
            }
        }
        self.active_attention_key = current_attention_key;

        if snapshot.conversation_epoch == previous_epoch
            && previous_phase.is_busy()
            && matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.paused_turn_id.is_none()
            && snapshot.retry.is_none()
            && let Some(turn_id) = previous_turn
        {
            self.notifications.push_unique(
                format!("turn-complete:{}:{turn_id}", snapshot.conversation_epoch),
                NotificationKind::Completed,
                text(Text::AgentTurnComplete),
                if snapshot.notice.is_none() {
                    text(Text::ReadyNextRequest).to_owned()
                } else {
                    notice_text(&snapshot.notice)
                },
            );
        }

        if let AgentPhase::Error {
            message,
            recoverable,
        } = &snapshot.phase
            && (previous_phase != &snapshot.phase
                || previous_phase_revision != snapshot.phase_revision)
        {
            self.notifications.push_unique(
                format!(
                    "error:{}:{}",
                    snapshot.conversation_epoch, snapshot.phase_revision
                ),
                NotificationKind::Error,
                if *recoverable {
                    text(Text::RecoverableError)
                } else {
                    text(Text::FatalError)
                },
                message.clone(),
            );
        }
        self.notification_ui.sync(&self.notifications);
    }

    pub fn handle_orchestrator_event(&mut self, event: OrchestratorEvent) {
        match event {
            OrchestratorEvent::PhaseChanged { turn_id, phase } => {
                let starts_new_turn = turn_id.is_some() && turn_id != self.active_turn_id;
                if self.phase != phase {
                    self.phase_started = Instant::now();
                }
                self.phase = phase;
                if starts_new_turn {
                    self.live_thinking.clear();
                    self.live_assistant.clear();
                    self.interrupted_draft.clear();
                    self.retry = None;
                }
                if let Some(turn_id) = turn_id {
                    self.active_turn_id = Some(turn_id);
                } else if matches!(&self.phase, AgentPhase::Idle) {
                    self.active_turn_id = None;
                }
            }
            OrchestratorEvent::ThinkingDelta { turn_id, delta } => {
                if self.accepts_turn_event(turn_id) {
                    self.active_turn_id = Some(turn_id);
                    self.live_thinking.push_str(&delta);
                }
            }
            OrchestratorEvent::AssistantCommitted { turn_id, content } => {
                if self.accepts_turn_event(turn_id) {
                    self.mascot.celebrate(Instant::now());
                    self.active_turn_id = Some(turn_id);
                    self.live_thinking.clear();
                    self.interrupted_draft.clear();
                    self.live_assistant = render::strip_service_blocks(&content);
                    self.status_message = Some(text(Text::AgentTurnComplete).to_owned());
                }
            }
            OrchestratorEvent::AssistantInterrupted { turn_id, content } => {
                if self.accepts_turn_event(turn_id) {
                    self.live_thinking.clear();
                    self.live_assistant.clear();
                    self.interrupted_draft = content;
                    self.status_message = Some(text(Text::InterruptedDraft).to_owned());
                }
            }
            OrchestratorEvent::ToolStarted {
                conversation_epoch,
                turn_id,
                action_id,
                action,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    self.eta
                        .tool_action_started(action_id, action.tool_name(), Instant::now());
                    Arc::make_mut(&mut self.tool_actions).insert(action_id, action.clone());
                    self.running_tools.insert(action_id);
                    self.pending_confirmation = self
                        .pending_confirmation
                        .take()
                        .filter(|pending| pending.action_id != action_id);
                    self.pending_mcp_confirmation = self
                        .pending_mcp_confirmation
                        .take()
                        .filter(|pending| pending.action_id != action_id);
                    self.pending_patch_review = self
                        .pending_patch_review
                        .take()
                        .filter(|pending| pending.action_id != action_id);
                    if self.pending_patch_review.is_none() {
                        self.patch_review_ui.close();
                    }
                    self.status_message = Some(format!(
                        "{} #{action_id}: {} {}",
                        text(Text::ToolLabel),
                        action.tool_name(),
                        text(Text::RunningTool)
                    ));
                }
            }
            OrchestratorEvent::ToolCompleted {
                conversation_epoch,
                turn_id,
                action_id,
                action,
                outcome,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    self.eta.tool_action_completed(action_id, Instant::now());
                    Arc::make_mut(&mut self.tool_actions).insert(action_id, action.clone());
                    self.running_tools.remove(&action_id);
                    self.status_message = Some(format_tool_outcome(action_id, &action, &outcome));
                }
            }
            OrchestratorEvent::McpToolStarted {
                conversation_epoch,
                turn_id,
                action_id,
                call,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    let tool_kind = format!("mcp:{}:{}", call.server, call.tool);
                    self.eta
                        .tool_action_started(action_id, &tool_kind, Instant::now());
                    Arc::make_mut(&mut self.mcp_calls).insert(action_id, Arc::clone(&call));
                    self.pending_mcp_confirmation = self
                        .pending_mcp_confirmation
                        .take()
                        .filter(|pending| pending.action_id != action_id);
                    self.status_message = Some(format!(
                        "MCP #{action_id}: {}::{} {}",
                        call.server,
                        call.tool,
                        text(Text::RunningTool)
                    ));
                }
            }
            OrchestratorEvent::McpToolCompleted {
                conversation_epoch,
                turn_id,
                action_id,
                call,
                outcome,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    self.eta.tool_action_completed(action_id, Instant::now());
                    Arc::make_mut(&mut self.mcp_calls).insert(action_id, Arc::clone(&call));
                    let state = if outcome.is_error {
                        text(Text::Failed)
                    } else {
                        text(Text::Completed)
                    };
                    self.status_message = Some(format!(
                        "MCP #{action_id}: {}::{} {state}",
                        call.server, call.tool
                    ));
                }
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
                if self.accepts_turn_event(turn_id) {
                    self.active_turn_id = Some(turn_id);
                    self.pending_mcp_confirmation = None;
                    self.pending_confirmation = Some(PendingConfirmation {
                        turn_id,
                        action_id,
                        action,
                        command,
                        command_bytes,
                        command_digest,
                        model_requested,
                        reason,
                        session_trust_available,
                    });
                    self.confirmation_ui.reset();
                    self.confirmation_scroll = 0;
                    self.confirmation_max_scroll = 0;
                    self.confirmation_view_ready = false;
                    self.confirmation_suffix_viewed = false;
                    self.confirmation_end_requested = false;
                    self.status_message = Some(text(Text::ConfirmShellCommand).to_owned());
                }
            }
            OrchestratorEvent::McpConfirmationRequested {
                turn_id,
                action_id,
                call,
                reason,
            } => {
                if self.accepts_turn_event(turn_id) {
                    self.active_turn_id = Some(turn_id);
                    self.pending_confirmation = None;
                    self.pending_mcp_confirmation = Some(PendingMcpConfirmation {
                        turn_id,
                        action_id,
                        call,
                        reason,
                    });
                    self.confirmation_ui.reset();
                    self.confirmation_scroll = 0;
                    self.confirmation_max_scroll = 0;
                    self.confirmation_view_ready = false;
                    self.confirmation_suffix_viewed = false;
                    self.confirmation_end_requested = false;
                    self.status_message = Some(text(Text::ApproveMcpTool).to_owned());
                }
            }
            OrchestratorEvent::PatchApprovalRequested {
                turn_id,
                action_id,
                review,
            } => {
                if self.accepts_turn_event(turn_id) {
                    self.active_turn_id = Some(turn_id);
                    let is_new_review = self.pending_patch_review.as_ref().is_none_or(|pending| {
                        pending.turn_id != turn_id || pending.action_id != action_id
                    });
                    if is_new_review {
                        self.patch_review_ui.open(review.hunks.len());
                    }
                    self.pending_patch_review = Some(PendingPatchReview {
                        turn_id,
                        action_id,
                        review,
                    });
                    self.shell_ui.select_tab(super::shell::ShellTab::Diff);
                    self.status_message = Some(text(Text::ReviewPatchHunks).to_owned());
                }
            }
            OrchestratorEvent::ContinuationRequested {
                turn_id,
                continuation_id,
                completed_iterations,
                max_iterations,
            } => {
                if self.accepts_turn_event(turn_id) {
                    self.active_turn_id = Some(turn_id);
                    self.pending_continuation = Some(PendingContinuation {
                        turn_id,
                        continuation_id,
                        completed_iterations,
                        max_iterations,
                    });
                    self.continuation_ui.reset();
                    self.status_message = Some(text(Text::ToolIterationLimit).to_owned());
                }
            }
            OrchestratorEvent::WhipAcknowledged {
                conversation_epoch,
                turn_id,
                kind,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    self.whip.acknowledge(&kind);
                    let label = match kind {
                        WhipKind::Soft => "soft",
                        WhipKind::Hard => "hard",
                    };
                    self.status_message = Some(format!(
                        "{} ({label}) #{turn_id}",
                        text(Text::WhipAcceptedRetry)
                    ));
                }
            }
            OrchestratorEvent::McpServersUpdated(servers) => {
                self.mcp_servers = servers;
                self.mcp_ui.sync(self.mcp_servers.as_ref());
                if let Some(prompt) = &self.mcp_oauth_prompt
                    && self.mcp_servers.iter().any(|server| {
                        server.name == prompt.server
                            && !matches!(
                                server.state,
                                crate::mcp::McpConnectionState::ReauthRequired
                                    | crate::mcp::McpConnectionState::Connecting
                            )
                    })
                {
                    self.mcp_oauth_prompt = None;
                }
            }
            OrchestratorEvent::McpOAuthPrompted(prompt) => {
                self.status_message = Some(format!(
                    "{}: {}",
                    text(Text::OAuthWaitingCallback),
                    prompt.server
                ));
                self.mcp_oauth_prompt = Some(prompt);
            }
            OrchestratorEvent::ResetAcknowledged { conversation_epoch } => {
                self.reset_view();
                self.conversation_epoch = conversation_epoch;
            }
            OrchestratorEvent::CheckpointsUpdated(checkpoints) => {
                self.checkpoints = checkpoints;
                self.rewind_ui.set_total(self.checkpoints.len());
            }
            OrchestratorEvent::RewindCompleted {
                conversation_epoch,
                report,
                history,
            } => {
                self.reset_view();
                self.conversation_epoch = conversation_epoch;
                self.history = history;
                self.status_message = Some(if report.preserved_conflicts.is_empty() {
                    format!(
                        "{} #{}: {} {} · {} {}",
                        text(Text::CheckpointRewind),
                        report.checkpoint_id,
                        report.restored_files.len(),
                        text(Text::FilesLabel),
                        report.restored_history_entries,
                        text(Text::HistoryLabel)
                    )
                } else {
                    format!(
                        "{} #{} · {}: {}",
                        text(Text::CheckpointRewind),
                        report.checkpoint_id,
                        text(Text::ManualEditsPreserved),
                        report.preserved_conflicts.len()
                    )
                });
            }
            OrchestratorEvent::SessionsUpdated {
                sessions,
                current_session_id,
            } => {
                self.sessions = sessions;
                self.session_ui.sync(&self.sessions);
                self.current_session_id = current_session_id;
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
                self.reset_view();
                self.conversation_epoch = conversation_epoch;
                self.history = history;
                self.current_session_id = Some(summary.id);
                self.tokens_input = usage.usage.input_tokens;
                self.tokens_output = usage.usage.output_tokens;
                self.tokens_total = usage.usage.total_tokens;
                self.usage = usage;
                self.usage_ui.sync(&self.usage);
                self.side_chat = side_chat;
                self.follow_ups = follow_ups;
                self.paused_turn_id = paused_turn_id;
                self.context_budget = context_budget;
                self.refresh_eta_context();
                self.follow_up_ui.sync(&self.follow_ups);
                self.status_message =
                    Some(format!("{}: {}", text(Text::ResumedSession), summary.title));
            }
            OrchestratorEvent::RuntimeSettingsUpdated {
                deployment,
                reasoning_effort,
                context_budget,
            } => {
                self.deployment = deployment;
                self.reasoning_effort = reasoning_effort;
                self.context_budget = context_budget;
                self.refresh_eta_context();
                let updated = format!(
                    "{}: {} / {} / {}K {}",
                    text(Text::RuntimeUpdated),
                    self.deployment,
                    self.reasoning_effort,
                    self.context_budget / 1_000,
                    text(Text::ContextSuffix)
                );
                self.status_message = Some(updated);
            }
            OrchestratorEvent::HistorySnapshot(history) => {
                self.history = history;
                if self.live_assistant_is_in_history() {
                    self.live_assistant.clear();
                }
            }
            OrchestratorEvent::Usage { turn_id: _, usage } => {
                self.tokens_input = usage.usage.input_tokens;
                self.tokens_output = usage.usage.output_tokens;
                self.tokens_total = usage.usage.total_tokens;
                self.usage = usage;
                self.usage_ui.sync(&self.usage);
            }
            OrchestratorEvent::BusyRejected { turn_id, message } => {
                self.status_message = Some(format!(
                    "{} #{turn_id}: {message}",
                    text(Text::BusyTurnRejected)
                ));
            }
            OrchestratorEvent::RetryScheduled {
                conversation_epoch,
                turn_id,
                next_attempt,
                max_attempts,
                reason,
            } => {
                if conversation_epoch == self.conversation_epoch && self.accepts_turn_event(turn_id)
                {
                    self.connection_status = text(Text::RetryScheduled).to_owned();
                    self.retry = Some(RetrySnapshot {
                        next_attempt,
                        max_attempts,
                        reason: reason.clone(),
                    });
                    self.status_message = Some(format!(
                        "{} {next_attempt}/{max_attempts}: {reason}",
                        text(Text::RetryScheduled)
                    ));
                }
            }
            OrchestratorEvent::RecoverableError {
                turn_id: _,
                message,
            } => {
                self.status_message = Some(format!("{}: {message}", text(Text::RecoverableError)));
            }
            OrchestratorEvent::FatalError { message } => {
                self.phase = AgentPhase::Error {
                    message: message.clone(),
                    recoverable: false,
                };
                self.status_message = Some(format!("{}: {message}", text(Text::FatalError)));
            }
            OrchestratorEvent::Done { turn_id } => {
                if self.accepts_turn_event(turn_id) {
                    self.eta.complete(turn_id, Instant::now());
                    self.live_thinking.clear();
                    self.pending_confirmation = None;
                    self.pending_mcp_confirmation = None;
                    self.pending_continuation = None;
                    self.continuation_ui.reset();
                    self.pending_patch_review = None;
                    self.patch_review_ui.close();
                    self.active_turn_id = None;
                    self.phase = AgentPhase::Idle;
                    self.retry = None;
                    self.phase_started = Instant::now();
                    self.status_message = Some(text(Text::Ready).to_owned());
                }
            }
            OrchestratorEvent::TurnPaused { turn_id } => {
                self.eta.suspend(Some(turn_id), Instant::now());
                self.live_thinking.clear();
                self.active_turn_id = None;
                self.paused_turn_id = Some(turn_id);
                self.phase = AgentPhase::Idle;
                self.phase_started = Instant::now();
                self.status_message = Some(format!(
                    "{} #{turn_id}; {}",
                    text(Text::TurnPausedSafely),
                    text(Text::ContinueDurableBoundary)
                ));
            }
        }
    }

    fn accepts_turn_event(&self, turn_id: TurnId) -> bool {
        self.active_turn_id == Some(turn_id)
    }

    fn live_assistant_is_in_history(&self) -> bool {
        self.live_assistant_is_in_history_for(self.active_turn_id)
    }

    fn live_assistant_is_in_history_for(&self, turn_id: Option<TurnId>) -> bool {
        if self.live_assistant.is_empty() {
            return false;
        }

        self.history.iter().rev().any(|entry| {
            matches!(&entry.kind, HistoryKind::Assistant)
                && matches!(&entry.status, HistoryStatus::Committed)
                && turn_id.is_none_or(|turn_id| entry.turn_id == turn_id)
                && render::strip_service_blocks(&entry.content) == self.live_assistant
        })
    }

    pub fn tick(&mut self, now: Instant) {
        self.eta.observe_stream_progress(
            self.live_thinking
                .len()
                .saturating_add(self.live_assistant.len()),
        );
        self.eta.observe(
            &self.phase,
            self.active_turn_id.or(self.paused_turn_id),
            now,
        );
        self.mascot.tick(now);
        self.whip.tick(now);
        self.terminal_ui.tick();
        self.mcp_ui.tick(now);
        self.lsp_ui.tick(now);
        self.code_index_ui.tick(now);
        self.privacy_ui.tick(now);
        self.permission_ui.tick(now);
        self.usage_ui.tick(now);
        self.side_chat_ui.tick(now);
        self.follow_up_ui.tick(now);
        self.review_ui.tick(now);
        self.notification_ui.tick(now);
        self.github_ui.tick(now);
        self.modes_ui.tick(now);
        self.instructions_ui.tick(now);
        self.skills_ui.tick(now);
        self.automation_ui.tick(now);
        self.plugin_ui.tick(now);
        self.approval_center_ui.tick(now);
    }

    fn refresh_eta_context(&mut self) {
        let (effort, _) = self.work_modes.effective_reasoning(self.reasoning_effort);
        self.eta.set_context(
            &self.provider,
            &self.deployment,
            &effort.to_string(),
            &self.work_modes.active_summary(),
        );
    }

    pub fn apply_terminal_snapshot(&mut self, snapshot: &TerminalFleetSnapshot) {
        self.terminal.clone_from(snapshot);
        self.terminal_ui.sync(snapshot);
        if let Some(notice) = &snapshot.notice {
            self.status_message = Some(terminal_notice_text(notice));
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

fn localized_phase_status(phase: &AgentPhase) -> String {
    match phase {
        AgentPhase::Idle => text(Text::Ready).to_owned(),
        AgentPhase::PreparingReview => text(Text::CapturingReviewSnapshot).to_owned(),
        AgentPhase::Planning => text(Text::BuildingImplementationPlan).to_owned(),
        AgentPhase::AwaitingPlanApproval => text(Text::WaitingPlanApproval).to_owned(),
        AgentPhase::Requesting => text(Text::SendingRequest).to_owned(),
        AgentPhase::Streaming => text(Text::WritingResponse).to_owned(),
        AgentPhase::Parsing => text(Text::ValidatingCompletedResponse).to_owned(),
        AgentPhase::ExecutingTools => text(Text::ExecutingTools).to_owned(),
        AgentPhase::AwaitingPatchApproval => text(Text::WaitingPatchApproval).to_owned(),
        AgentPhase::AwaitingConfirmation => text(Text::WaitingCommandApproval).to_owned(),
        AgentPhase::AwaitingContinuation => text(Text::WaitingContinuationApproval).to_owned(),
        AgentPhase::Error {
            message,
            recoverable,
        } => format!(
            "{}: {message}",
            text(if *recoverable {
                Text::RecoverableError
            } else {
                Text::FatalError
            })
        ),
    }
}

fn format_tool_outcome(action_id: ActionId, action: &ToolAction, outcome: &ToolOutcome) -> String {
    match outcome {
        ToolOutcome::Success(_) => format!(
            "{} #{action_id}: {} {}",
            text(Text::ToolLabel),
            action.tool_name(),
            text(Text::Completed)
        ),
        ToolOutcome::Failure { message } => format!(
            "{} #{action_id}: {} {}: {message}",
            text(Text::ToolLabel),
            action.tool_name(),
            text(Text::Failed)
        ),
        ToolOutcome::Declined { .. } => format!(
            "{} #{action_id}: {} {}",
            text(Text::ToolLabel),
            action.tool_name(),
            text(Text::Declined)
        ),
    }
}

pub async fn run_app(config: AppConfig) -> Result<(), AppError> {
    super::i18n::set_language(config.ui.language);
    let (event_tx, mut event_rx) = mpsc::channel::<OrchestratorEvent>(EVENT_CHANNEL_CAPACITY);
    let (cmd_tx, cmd_rx) = mpsc::channel::<OrchestratorCommand>(COMMAND_CHANNEL_CAPACITY);

    let (orchestrator, mut snapshot_rx, urgent_control) = Orchestrator::with_runtime_and_mcp(
        config.api.clone(),
        config.agent.clone(),
        config.mcp.clone(),
        config.lsp.clone(),
        config.code_index.clone(),
        config.github.clone(),
        event_tx,
        cmd_rx,
    )?;

    let mouse_enabled = config.ui.mouse_enabled;
    let (terminal_control, mut terminal_snapshot_rx, mut terminal_runtime_task) =
        start_terminal_runtime(
            config.agent.shell.terminal.clone(),
            config.agent.workspace_root.clone(),
        );
    let workspace_for_scan = config.agent.workspace_root.clone();
    let workspace_files =
        match tokio::task::spawn_blocking(move || scan_workspace_files(&workspace_for_scan, 5_000))
            .await
        {
            Ok(files) => files,
            Err(error) => {
                tracing::warn!(%error, "workspace file sidebar scan failed");
                Vec::new()
            }
        };
    let mut terminal = TerminalSession::start(mouse_enabled)?;
    let mut orchestrator_task = tokio::spawn(orchestrator.run());
    let mut state = AppState::new();
    state.provider = config.api.provider.label().to_owned();
    state.eta = EtaTracker::load(config.agent.session_dir.join("eta-history.json"));
    state.language = config.ui.language;
    state.mascot = MascotState::new(config.ui.mascot_enabled);
    state.show_thinking = config.ui.show_thinking;
    state.show_tool_activity = config.ui.show_tool_activity;
    state
        .workspace_root
        .clone_from(&config.agent.workspace_root);
    state.workspace_files = Arc::from(workspace_files);
    state.terminal_ui.attach_control(terminal_control.clone());
    state.deployment_choices = Arc::from(config.deployment_choices.clone());
    state.context_mode = match config.agent.context_mode {
        ContextMode::Stateless => "stateless",
        ContextMode::Stateful => "stateful",
    };
    state.context_budget = config.agent.context_budget;
    state.max_context_budget = config.agent.max_context_budget;
    state.whip_enabled = config.agent.whip.enabled;
    if let Some(hotkey) = config.agent.whip.hotkey.chars().next() {
        state.whip_hotkey = hotkey;
    }

    state.apply_snapshot(&snapshot_rx.borrow_and_update().clone());
    state.apply_terminal_snapshot(&terminal_snapshot_rx.borrow_and_update().clone());
    let result = run_main_loop(
        terminal.terminal_mut(),
        &mut state,
        &mut snapshot_rx,
        &mut terminal_snapshot_rx,
        &mut event_rx,
        &cmd_tx,
        &urgent_control,
        mouse_enabled,
    )
    .await;

    urgent_control.shutdown();
    terminal_control.shutdown().await;
    let restore_result = terminal.restore();
    let task_result =
        match tokio::time::timeout(ORCHESTRATOR_SHUTDOWN_GRACE, &mut orchestrator_task).await {
            Ok(result) => result.map_err(|error| AppError::OrchestratorTask(error.to_string())),
            Err(_) => {
                orchestrator_task.abort();
                match orchestrator_task.await {
                    Ok(()) => Ok(()),
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(AppError::OrchestratorTask(error.to_string())),
                }
            }
        };
    let terminal_task_result =
        match tokio::time::timeout(ORCHESTRATOR_SHUTDOWN_GRACE, &mut terminal_runtime_task).await {
            Ok(result) => result
                .map_err(|error| AppError::Terminal(format!("{}: {error}", text(Text::Terminal)))),
            Err(_) => {
                terminal_runtime_task.abort();
                match terminal_runtime_task.await {
                    Ok(()) => Ok(()),
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(AppError::Terminal(format!(
                        "interactive terminal runtime failed: {error}"
                    ))),
                }
            }
        };
    let force_aborted = crate::tools::drain_deferred_reapers(ORCHESTRATOR_SHUTDOWN_GRACE).await;
    if force_aborted > 0 {
        tracing::warn!(
            force_aborted,
            "force-aborted deferred process reapers during TUI shutdown"
        );
    }

    prioritize_run_results(result, task_result, terminal_task_result, restore_result)
}

pub async fn run_onboarding() -> Result<super::onboarding::WizardOutcome, AppError> {
    let mut terminal = TerminalSession::start(true)?;
    let result = super::onboarding::run(terminal.terminal_mut()).await;
    let restore = terminal.restore();
    match (result, restore) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn scan_workspace_files(root: &std::path::Path, limit: usize) -> Vec<String> {
    ignore::WalkBuilder::new(root)
        .follow_links(false)
        .standard_filters(true)
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        })
        .take(limit)
        .collect()
}

fn prioritize_run_results(
    main_loop: Result<(), AppError>,
    orchestrator_task: Result<(), AppError>,
    terminal_task: Result<(), AppError>,
    terminal_restore: Result<(), AppError>,
) -> Result<(), AppError> {
    orchestrator_task?;
    terminal_task?;
    terminal_restore?;
    main_loop
}

#[allow(clippy::too_many_arguments)]
async fn run_main_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state: &mut AppState,
    snapshot_rx: &mut tokio::sync::watch::Receiver<UiSnapshot>,
    terminal_snapshot_rx: &mut tokio::sync::watch::Receiver<TerminalFleetSnapshot>,
    event_rx: &mut mpsc::Receiver<OrchestratorEvent>,
    cmd_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: &UrgentControlHandle,
    mouse_enabled: bool,
) -> Result<(), AppError> {
    use crossterm::event::{Event, EventStream};
    use futures_util::StreamExt;

    let mut reader = EventStream::new();
    let mut animation_tick = tokio::time::interval(Duration::from_millis(50));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_mcp_oauth_poll = Instant::now();
    let mut last_lsp_refresh = Instant::now();
    let mut last_code_index_poll = Instant::now();
    let mut terminal_channel_open = true;
    let mut last_terminal_title = String::new();

    loop {
        let terminal_title = render::terminal_title(state);
        if terminal_title != last_terminal_title {
            execute!(terminal.backend_mut(), SetTitle(&terminal_title))
                .map_err(|error| AppError::Terminal(error.to_string()))?;
            last_terminal_title = terminal_title;
        }
        terminal
            .draw(|frame| render::draw(frame, state))
            .map_err(|error| AppError::Terminal(error.to_string()))?;
        if let Err(error) = state.terminal_ui.flush_pending_resize() {
            state.status_message = Some(terminal_control_error_text(&error));
        }

        tokio::select! {
            orchestrator_event = event_rx.recv() => {
                match orchestrator_event {
                    Some(event @ (OrchestratorEvent::WhipAcknowledged { .. }
                        | OrchestratorEvent::ToolStarted { .. }
                        | OrchestratorEvent::ToolCompleted { .. }
                        | OrchestratorEvent::RetryScheduled { .. })) => {
                        state.handle_orchestrator_event(event);
                    }
                    Some(_) => {}
                    None => {
                        state.connected = false;
                        state.status_message = Some(format!(
                            "{}: {}",
                            text(Text::Agent),
                            text(Text::ClosedStatus)
                        ));
                        break;
                    }
                }
            }
            terminal_event = reader.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) => {
                        input::handle_key_with_control(key, state, cmd_tx, urgent_control);
                    }
                    Some(Ok(Event::Paste(text))) => {
                        input::handle_paste_with_commands(&text, state, cmd_tx);
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        input::handle_mouse_enabled(
                            mouse,
                            state,
                            cmd_tx,
                            urgent_control,
                            mouse_enabled,
                        );
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        return Err(AppError::Terminal(error.to_string()));
                    }
                    None => break,
                }
            }
            _ = animation_tick.tick() => {
                match snapshot_rx.has_changed() {
                    Ok(true) => {
                        let snapshot = snapshot_rx.borrow_and_update().clone();
                        state.apply_snapshot(&snapshot);
                    }
                    Ok(false) => {}
                    Err(_) => {
                        state.connected = false;
                        state.status_message = Some(format!(
                            "{}: {}",
                            text(Text::Agent),
                            text(Text::ClosedStatus)
                        ));
                        break;
                    }
                }
                if terminal_channel_open {
                    match terminal_snapshot_rx.has_changed() {
                        Ok(true) => {
                            let snapshot = terminal_snapshot_rx.borrow_and_update().clone();
                            state.apply_terminal_snapshot(&snapshot);
                        }
                        Ok(false) => {}
                        Err(_) => {
                            terminal_channel_open = false;
                            state.status_message = Some(format!(
                                "{}: {} · {}: {}",
                                text(Text::Terminal),
                                text(Text::ExitedStatus),
                                text(Text::Agent),
                                text(Text::Ready)
                            ));
                        }
                    }
                }
                let now = Instant::now();
                state.tick(now);
                if now.saturating_duration_since(last_mcp_oauth_poll)
                    >= Duration::from_millis(400)
                    && let Some(prompt) = &state.mcp_oauth_prompt
                {
                    let _ = cmd_tx.try_send(OrchestratorCommand::McpPollOAuth {
                        server: prompt.server.clone(),
                        scope: crate::agent::orchestrator::CommandScope {
                            conversation_epoch: state.conversation_epoch,
                            phase_revision: state.phase_revision,
                        },
                    });
                    last_mcp_oauth_poll = now;
                }
                if now.saturating_duration_since(last_lsp_refresh)
                    >= Duration::from_millis(400)
                    && state.lsp_ui.is_open()
                    && matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
                {
                    let _ = cmd_tx.try_send(OrchestratorCommand::LspRefresh {
                        scope: crate::agent::orchestrator::CommandScope {
                            conversation_epoch: state.conversation_epoch,
                            phase_revision: state.phase_revision,
                        },
                    });
                    last_lsp_refresh = now;
                }
                if now.saturating_duration_since(last_code_index_poll)
                    >= Duration::from_millis(250)
                    && state.code_index_ui.is_open()
                    && matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
                {
                    let _ = cmd_tx.try_send(OrchestratorCommand::CodeIndexPoll {
                        scope: crate::agent::orchestrator::CommandScope {
                            conversation_epoch: state.conversation_epoch,
                            phase_revision: state.phase_revision,
                        },
                    });
                    last_code_index_poll = now;
                }
            }
        }

        if state.notifications.take_bell_pending() {
            terminal
                .backend_mut()
                .write_all(b"\x07")
                .and_then(|()| terminal.backend_mut().flush())
                .map_err(|error| AppError::Terminal(error.to_string()))?;
        }

        if state.should_quit {
            return Err(AppError::UserExit);
        }
    }

    Ok(())
}

#[derive(Debug, Default)]
struct CleanupFlags {
    raw: AtomicBool,
    alternate: AtomicBool,
    paste: AtomicBool,
    mouse: AtomicBool,
    cursor_hidden: AtomicBool,
}

#[derive(Debug, Clone, Default)]
struct TerminalCleanup {
    flags: Arc<CleanupFlags>,
}

trait CleanupOps {
    fn disable_paste(&mut self) -> io::Result<()>;
    fn disable_mouse(&mut self) -> io::Result<()>;
    fn leave_alternate(&mut self) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn disable_raw(&mut self) -> io::Result<()>;
}

struct SystemCleanup {
    stdout: io::Stdout,
}

impl CleanupOps for SystemCleanup {
    fn disable_paste(&mut self) -> io::Result<()> {
        execute!(self.stdout, DisableBracketedPaste)
    }

    fn disable_mouse(&mut self) -> io::Result<()> {
        execute!(self.stdout, DisableMouseCapture)
    }

    fn leave_alternate(&mut self) -> io::Result<()> {
        execute!(self.stdout, LeaveAlternateScreen)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.stdout, Show)
    }

    fn disable_raw(&mut self) -> io::Result<()> {
        disable_raw_mode()
    }
}

impl TerminalCleanup {
    fn set_raw(&self) {
        self.flags.raw.store(true, Ordering::Release);
    }

    fn set_alternate(&self) {
        self.flags.alternate.store(true, Ordering::Release);
    }

    fn set_mouse(&self) {
        self.flags.mouse.store(true, Ordering::Release);
    }

    fn set_paste(&self) {
        self.flags.paste.store(true, Ordering::Release);
    }

    fn restore(&self) -> io::Result<()> {
        let mut operations = SystemCleanup {
            stdout: io::stdout(),
        };
        self.restore_with_retry(&mut operations)
    }

    fn restore_with_retry(&self, operations: &mut impl CleanupOps) -> io::Result<()> {
        let first = self.restore_once(operations);
        if first.is_ok() {
            return first;
        }
        self.restore_once(operations)
    }

    fn restore_once(&self, operations: &mut impl CleanupOps) -> io::Result<()> {
        let mut first_error = None;

        if self.flags.paste.load(Ordering::Acquire) {
            match operations.disable_paste() {
                Ok(()) => self.flags.paste.store(false, Ordering::Release),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if self.flags.mouse.load(Ordering::Acquire) {
            match operations.disable_mouse() {
                Ok(()) => self.flags.mouse.store(false, Ordering::Release),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if self.flags.alternate.load(Ordering::Acquire) {
            match operations.leave_alternate() {
                Ok(()) => self.flags.alternate.store(false, Ordering::Release),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if self.flags.cursor_hidden.load(Ordering::Acquire) {
            match operations.show_cursor() {
                Ok(()) => self.flags.cursor_hidden.store(false, Ordering::Release),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        } else if let Err(error) = operations.show_cursor()
            && first_error.is_none()
        {
            first_error = Some(error);
        }
        if self.flags.raw.load(Ordering::Acquire) {
            match operations.disable_raw() {
                Ok(()) => self.flags.raw.store(false, Ordering::Release),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }

        first_error.map_or(Ok(()), Err)
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    cleanup: TerminalCleanup,
    previous_panic_hook: Arc<Mutex<Option<PanicHook>>>,
    panic_hook_installed: bool,
}

impl TerminalSession {
    fn start(mouse_enabled: bool) -> Result<Self, AppError> {
        // Install the panic hook before the first terminal transition. The
        // individual flags below are set only after each transition succeeds,
        // so partial setup is recoverable and failed cleanup is retried in Drop.
        let cleanup = TerminalCleanup::default();
        let previous_panic_hook = Arc::new(Mutex::new(Some(std::panic::take_hook())));
        let hook_state = Arc::clone(&previous_panic_hook);
        let hook_cleanup = cleanup.clone();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = hook_cleanup.restore();
            if let Ok(previous) = hook_state.lock()
                && let Some(previous) = previous.as_ref()
            {
                previous(panic_info);
            }
        }));

        if let Err(error) = enable_raw_mode() {
            restore_panic_hook(&previous_panic_hook);
            return Err(AppError::Terminal(error.to_string()));
        }
        cleanup.set_raw();
        let mut stdout = io::stdout();

        if let Err(error) = execute!(stdout, SetTitle("DEcode by denysoid"), EnterAlternateScreen) {
            let _ = cleanup.restore();
            restore_panic_hook(&previous_panic_hook);
            return Err(AppError::Terminal(error.to_string()));
        }
        cleanup.set_alternate();
        if let Err(error) = execute!(stdout, EnableBracketedPaste) {
            let _ = cleanup.restore();
            restore_panic_hook(&previous_panic_hook);
            return Err(AppError::Terminal(error.to_string()));
        }
        cleanup.set_paste();
        if mouse_enabled {
            if let Err(error) = execute!(stdout, EnableMouseCapture) {
                let _ = cleanup.restore();
                restore_panic_hook(&previous_panic_hook);
                return Err(AppError::Terminal(error.to_string()));
            }
            cleanup.set_mouse();
        }

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = cleanup.restore();
                restore_panic_hook(&previous_panic_hook);
                return Err(AppError::Terminal(error.to_string()));
            }
        };

        Ok(Self {
            terminal,
            cleanup,
            previous_panic_hook,
            panic_hook_installed: true,
        })
    }

    fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<io::Stdout>> {
        &mut self.terminal
    }

    fn restore(mut self) -> Result<(), AppError> {
        let title_result = execute!(self.terminal.backend_mut(), SetTitle("DEcode by denysoid"))
            .map_err(|error| AppError::Terminal(error.to_string()));
        let cursor_result = self
            .terminal
            .show_cursor()
            .map_err(|error| AppError::Terminal(error.to_string()));
        let terminal_result = self
            .cleanup
            .restore()
            .map_err(|error| AppError::Terminal(error.to_string()));
        self.restore_panic_hook();

        title_result?;
        cursor_result?;
        terminal_result
    }

    fn restore_panic_hook(&mut self) {
        if !self.panic_hook_installed {
            return;
        }
        self.panic_hook_installed = false;
        restore_panic_hook(&self.previous_panic_hook);
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = self.cleanup.restore();
        self.restore_panic_hook();
    }
}

fn restore_panic_hook(previous_panic_hook: &Arc<Mutex<Option<PanicHook>>>) {
    let _installed_hook = std::panic::take_hook();
    if let Ok(mut previous) = previous_panic_hook.lock()
        && let Some(previous) = previous.take()
    {
        std::panic::set_hook(previous);
    }
}

#[cfg(test)]
mod tests {
    use super::{AppState, CleanupOps, TerminalCleanup, prioritize_run_results};
    use crate::{
        agent::{
            AgentProfileCatalogSnapshot, AgentState, PlanReview, SubagentFileReview,
            SubagentFleetSnapshot, SubagentId, SubagentMode, SubagentPendingCommand,
            SubagentSnapshot, SubagentStatus,
            orchestrator::{UiModal, UiSnapshot},
            phase::AgentPhase,
        },
        api::ReasoningEffort,
        error::AppError,
        notice::UiNotice,
        ui::agents::AgentDecisionFocus,
    };
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};
    use std::io;
    use std::sync::{Arc, atomic::Ordering};

    #[derive(Default)]
    struct InjectedCleanup {
        paste_calls: usize,
        mouse_calls: usize,
        alternate_calls: usize,
        cursor_calls: usize,
        raw_calls: usize,
        fail_first: bool,
    }

    impl InjectedCleanup {
        fn result(calls: usize, fail_first: bool) -> io::Result<()> {
            if fail_first && calls == 1 {
                Err(io::Error::other("injected cleanup failure"))
            } else {
                Ok(())
            }
        }
    }

    impl CleanupOps for InjectedCleanup {
        fn disable_paste(&mut self) -> io::Result<()> {
            self.paste_calls += 1;
            Self::result(self.paste_calls, self.fail_first)
        }

        fn disable_mouse(&mut self) -> io::Result<()> {
            self.mouse_calls += 1;
            Self::result(self.mouse_calls, self.fail_first)
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            self.alternate_calls += 1;
            Self::result(self.alternate_calls, self.fail_first)
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.cursor_calls += 1;
            Ok(())
        }

        fn disable_raw(&mut self) -> io::Result<()> {
            self.raw_calls += 1;
            Self::result(self.raw_calls, self.fail_first)
        }
    }

    struct DistinctCleanupFailures;

    impl CleanupOps for DistinctCleanupFailures {
        fn disable_paste(&mut self) -> io::Result<()> {
            Err(io::Error::other("paste cleanup failed"))
        }

        fn disable_mouse(&mut self) -> io::Result<()> {
            Err(io::Error::other("mouse cleanup failed"))
        }

        fn leave_alternate(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn disable_raw(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cleanup_tracks_each_terminal_transition_separately() {
        let cleanup = TerminalCleanup::default();
        cleanup.set_raw();
        cleanup.set_alternate();
        cleanup.set_paste();
        cleanup.set_mouse();

        assert!(cleanup.flags.raw.load(Ordering::Acquire));
        assert!(cleanup.flags.alternate.load(Ordering::Acquire));
        assert!(cleanup.flags.paste.load(Ordering::Acquire));
        assert!(cleanup.flags.mouse.load(Ordering::Acquire));
    }

    #[test]
    fn partial_cleanup_failures_are_retried_with_injected_operations() {
        let cleanup = TerminalCleanup::default();
        cleanup.set_raw();
        cleanup.set_alternate();
        cleanup.set_paste();
        cleanup.set_mouse();
        let mut operations = InjectedCleanup {
            fail_first: true,
            ..InjectedCleanup::default()
        };

        assert!(cleanup.restore_with_retry(&mut operations).is_ok());
        assert_eq!(operations.paste_calls, 2);
        assert_eq!(operations.mouse_calls, 2);
        assert_eq!(operations.alternate_calls, 2);
        assert_eq!(operations.raw_calls, 2);
        assert!(!cleanup.flags.raw.load(Ordering::Acquire));
        assert!(!cleanup.flags.alternate.load(Ordering::Acquire));
        assert!(!cleanup.flags.paste.load(Ordering::Acquire));
        assert!(!cleanup.flags.mouse.load(Ordering::Acquire));
    }

    #[test]
    fn cleanup_reports_the_first_failure() {
        let cleanup = TerminalCleanup::default();
        cleanup.set_paste();
        cleanup.set_mouse();

        let result = cleanup.restore_once(&mut DistinctCleanupFailures);

        assert!(matches!(result, Err(error) if error.to_string() == "paste cleanup failed"));
    }

    #[test]
    fn replacing_a_subagent_command_resets_approval_focus() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut state = AppState::new();
        let first = UiSnapshot {
            subagents: fleet_with_agent(agent_with_command(1)),
            ..UiSnapshot::default()
        };
        state.apply_snapshot(&first);
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        let agent = state.subagents.agents[0].clone();
        terminal.draw(|frame| state.agents_ui.draw_command_approval(frame, &agent))?;
        state.agents_ui.focus_command(AgentDecisionFocus::Approve);

        let second = UiSnapshot {
            phase_revision: 1,
            subagents: fleet_with_agent(agent_with_command(2)),
            ..UiSnapshot::default()
        };
        state.apply_snapshot(&second);
        let agent = state.subagents.agents[0].clone();
        terminal.draw(|frame| state.agents_ui.draw_command_approval(frame, &agent))?;

        assert_eq!(
            state.agents_ui.command_focused(),
            Some(AgentDecisionFocus::Decline)
        );
        Ok(())
    }

    #[test]
    fn replacing_a_binary_review_resets_approval_focus() -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AppState::new();
        let first_review = binary_review("first.bin");
        let first = UiSnapshot {
            modal: Some(UiModal::SubagentPatchApproval {
                review: Arc::new(first_review.clone()),
            }),
            ..UiSnapshot::default()
        };
        state.apply_snapshot(&first);
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        terminal.draw(|frame| state.agents_ui.draw_binary_review(frame, &first_review))?;
        state.agents_ui.focus_binary(AgentDecisionFocus::Approve);

        let second_review = binary_review("second.bin");
        let second = UiSnapshot {
            phase_revision: 1,
            modal: Some(UiModal::SubagentPatchApproval {
                review: Arc::new(second_review.clone()),
            }),
            ..UiSnapshot::default()
        };
        state.apply_snapshot(&second);
        terminal.draw(|frame| state.agents_ui.draw_binary_review(frame, &second_review))?;

        assert_eq!(
            state.agents_ui.binary_focused(),
            Some(AgentDecisionFocus::Decline)
        );
        Ok(())
    }

    #[test]
    fn shutdown_result_prefers_task_and_cleanup_failures_over_user_exit() {
        let result = prioritize_run_results(
            Err(AppError::UserExit),
            Err(AppError::OrchestratorTask("panic".to_owned())),
            Ok(()),
            Err(AppError::Terminal("restore".to_owned())),
        );
        assert!(matches!(result, Err(AppError::OrchestratorTask(message)) if message == "panic"));

        let result = prioritize_run_results(
            Err(AppError::UserExit),
            Ok(()),
            Ok(()),
            Err(AppError::Terminal("restore".to_owned())),
        );
        assert!(matches!(result, Err(AppError::Terminal(message)) if message == "restore"));
    }

    #[test]
    fn authoritative_snapshots_deduplicate_attention_and_notify_completion_once() {
        let mut state = AppState::new();
        state.apply_snapshot(&crate::agent::orchestrator::UiSnapshot::default());
        assert!(state.notifications.items().is_empty());

        let requesting = crate::agent::orchestrator::UiSnapshot {
            phase: AgentPhase::Requesting,
            phase_revision: 1,
            active_turn_id: Some(7),
            ..crate::agent::orchestrator::UiSnapshot::default()
        };
        state.apply_snapshot(&requesting);
        let approval = crate::agent::orchestrator::UiSnapshot {
            phase: AgentPhase::AwaitingPlanApproval,
            phase_revision: 2,
            active_turn_id: Some(7),
            modal: Some(UiModal::PlanApproval {
                review: Arc::new(PlanReview {
                    turn_id: 7,
                    review_id: 11,
                    plan: "Inspect, change, verify".to_owned(),
                    deployment: "test".to_owned(),
                    reasoning_effort: ReasoningEffort::XHigh,
                    reasoning_mode: None,
                }),
            }),
            ..crate::agent::orchestrator::UiSnapshot::default()
        };
        state.apply_snapshot(&approval);
        state.apply_snapshot(&approval);
        assert_eq!(state.notifications.items().len(), 1);
        assert_eq!(state.notifications.unread_count(), 1);
        assert!(state.notifications.take_bell_pending());
        assert!(!state.notifications.take_bell_pending());

        let completed = crate::agent::orchestrator::UiSnapshot {
            phase: AgentPhase::Idle,
            phase_revision: 3,
            status: "Plan rejected safely".to_owned(),
            ..crate::agent::orchestrator::UiSnapshot::default()
        };
        state.apply_snapshot(&completed);
        state.apply_snapshot(&completed);
        assert_eq!(state.notifications.items().len(), 2);
        assert_eq!(state.notifications.unread_count(), 1);
        assert!(state.notifications.take_bell_pending());
        assert!(!state.notifications.take_bell_pending());
    }

    #[test]
    fn error_snapshot_rings_once_per_phase_revision() {
        let mut state = AppState::new();
        state.apply_snapshot(&crate::agent::orchestrator::UiSnapshot::default());
        let failed = crate::agent::orchestrator::UiSnapshot {
            phase: AgentPhase::Error {
                message: "network retry budget exhausted".to_owned(),
                recoverable: true,
            },
            phase_revision: 1,
            ..crate::agent::orchestrator::UiSnapshot::default()
        };
        state.apply_snapshot(&failed);
        state.apply_snapshot(&failed);
        assert_eq!(state.notifications.items().len(), 1);
        assert_eq!(state.notifications.unread_count(), 1);
        assert!(state.notifications.take_bell_pending());
        assert!(!state.notifications.take_bell_pending());
    }

    #[test]
    fn paused_turn_is_not_reported_as_completed() {
        let mut state = AppState::new();
        state.apply_snapshot(&UiSnapshot::default());
        state.apply_snapshot(&UiSnapshot {
            phase: AgentPhase::Streaming,
            phase_revision: 1,
            active_turn_id: Some(9),
            ..UiSnapshot::default()
        });

        state.apply_snapshot(&UiSnapshot {
            phase: AgentPhase::Idle,
            phase_revision: 2,
            paused_turn_id: Some(9),
            ..UiSnapshot::default()
        });

        assert!(state.notifications.items().is_empty());
        assert!(!state.notifications.take_bell_pending());
    }

    #[test]
    fn authoritative_history_never_renders_the_same_committed_assistant_twice() {
        let mut history = AgentState::new();
        history.push_user(7, "hello".to_owned());
        history.push_assistant(7, "same answer".to_owned());
        let snapshot = UiSnapshot {
            active_turn_id: Some(7),
            history_revision: 1,
            history: history.history.into(),
            assistant: "same answer".to_owned(),
            ..UiSnapshot::default()
        };
        let mut state = AppState::new();
        state.apply_snapshot(&snapshot);
        assert!(state.live_assistant.is_empty());
        assert_eq!(state.history.len(), 2);
    }

    #[test]
    fn legacy_backend_status_is_never_rendered_as_unlocalized_ui_copy() {
        let mut state = AppState::new();
        let snapshot = UiSnapshot {
            status: "legacy English status".to_owned(),
            ..UiSnapshot::default()
        };

        state.apply_snapshot(&snapshot);

        assert_ne!(
            state.status_message.as_deref(),
            Some("legacy English status")
        );
    }

    fn fleet_with_agent(agent: SubagentSnapshot) -> SubagentFleetSnapshot {
        SubagentFleetSnapshot {
            revision: 1,
            enabled: true,
            capacity: 1,
            active: 1,
            total_tokens: 0,
            token_budget: 1,
            availability_error: None,
            mcp_enabled: false,
            mcp_status: UiNotice::SubagentMcpDisabled,
            profiles: AgentProfileCatalogSnapshot::default(),
            agents: Arc::from([agent]),
        }
    }

    fn agent_with_command(action_id: u64) -> SubagentSnapshot {
        let now = Utc::now();
        SubagentSnapshot {
            id: SubagentId::new(1),
            parent_id: None,
            depth: 1,
            revision: action_id,
            session_id: None,
            label: "agent".to_owned(),
            task: "task".to_owned(),
            profile_id: "builtin:research".to_owned(),
            profile_name: "Research".to_owned(),
            mode: SubagentMode::Research,
            status: SubagentStatus::WaitingApproval,
            deployment: "model".to_owned(),
            reasoning_effort: ReasoningEffort::High,
            created_at: now,
            started_at: Some(now),
            completed_at: None,
            updated_at: now,
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            token_budget: 1,
            tool_iterations: 1,
            last_message: String::new(),
            result: String::new(),
            error: None,
            worktree: None,
            base_commit: None,
            changed_files: Arc::from([]),
            resolved_files: Arc::from([]),
            change_digest: None,
            pending_command: Some(SubagentPendingCommand {
                action_id,
                command: "cargo check".to_owned(),
                model_requested_confirmation: false,
                mcp: false,
            }),
            pending_budget: None,
            transcript: Arc::from([]),
            recovery: None,
            dependencies: Arc::from([]),
            file_claims: Arc::from([]),
        }
    }

    fn binary_review(path: &str) -> SubagentFileReview {
        SubagentFileReview {
            agent_id: SubagentId::new(1),
            agent_revision: 1,
            change_digest: "digest".to_owned(),
            path: path.to_owned(),
            binary: true,
            review: None,
        }
    }
}
