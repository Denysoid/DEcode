use std::collections::{BTreeMap, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    attachments::AttachmentRef,
    parser::{ToolAction, ToolOutcome},
    usage::UsageLedger,
};

use super::{
    approval::AutoApprovalPolicy, followups::FollowUpState, modes::WorkModes, review::ReviewState,
    side_chat::SideChatState,
};

pub type TurnId = u64;
pub type ActionId = u64;
pub type ContinuationId = u64;

const MAX_PERSISTED_API_ENTRIES: usize = 2_048;
const MAX_PERSISTED_API_BYTES: usize = 2 * 1024 * 1024;
const MAX_PERSISTED_DRAFTS: usize = 64;
const MAX_PERSISTED_DRAFT_BYTES: usize = 16 * 1024;
const MAX_TRANSCRIPT_ARCHIVE_ENTRIES: usize = 512;
const MAX_TRANSCRIPT_ARCHIVE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TRANSCRIPT_ENTRY_BYTES: usize = 128 * 1024;
const MAX_CONTEXT_CAPSULE_TOKENS: u64 = 4_096;
const MAX_CONTEXT_CAPSULE_TURNS: usize = 24;
const MAX_CONTEXT_CAPSULE_EXCERPT_CHARS: usize = 320;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBudgetExceeded {
    pub required_tokens: u32,
    pub context_budget: u32,
}

impl std::fmt::Display for ContextBudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "request context requires approximately {} tokens but context budget is {}",
            self.required_tokens, self.context_budget
        )
    }
}

impl std::error::Error for ContextBudgetExceeded {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryStatus {
    /// Accepted for the current logical turn but not yet confirmed by a
    /// completed model response.
    Pending,
    /// Explicit user pause at a durable orchestration boundary. Unlike an
    /// interrupted turn this may be resumed after process restart.
    Paused,
    Committed,
    Interrupted,
    /// A partial assistant attempt replaced by a later retry.
    Superseded,
    /// A partial assistant attempt that ended in a terminal failure.
    Failed,
    /// Work explicitly cancelled before it became committed context.
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Failure,
    Declined,
    ParseError,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HistoryKind {
    User,
    Assistant,
    ToolResult {
        action_id: ActionId,
        tool_name: String,
        outcome: ToolResultStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnMetrics {
    pub elapsed_millis: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub cost_microusd: Option<u64>,
}

impl TurnMetrics {
    fn add_assign(&mut self, segment: &Self) {
        self.elapsed_millis = self.elapsed_millis.saturating_add(segment.elapsed_millis);
        self.input_tokens = self.input_tokens.saturating_add(segment.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(segment.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(segment.total_tokens);
        self.cost_microusd = match (self.cost_microusd, segment.cost_microusd) {
            (Some(total), Some(cost)) => Some(total.saturating_add(cost)),
            _ => None,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolActionSummary {
    pub tool_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl ToolActionSummary {
    #[must_use]
    pub fn from_action(action: &ToolAction) -> Self {
        let target = match action {
            ToolAction::ReadFile { path }
            | ToolAction::ListDirectory { path }
            | ToolAction::WriteFile { path, .. }
            | ToolAction::ApplyPatch { path, .. } => Some(path.clone()),
            ToolAction::SearchCode { path, .. } => path.clone(),
            ToolAction::ExecuteCommand { .. } => None,
        };
        Self {
            tool_name: action.tool_name().to_owned(),
            target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    #[serde(default = "default_history_epoch")]
    pub epoch: u64,
    #[serde(default)]
    pub revision: u64,
    pub sequence: u64,
    pub turn_id: TurnId,
    pub kind: HistoryKind,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<AttachmentRef>,
    pub status: HistoryStatus,
    pub approx_tokens: u32,
    pub created_at: DateTime<Utc>,
    /// Lossless Responses API items associated with this history entry. These
    /// are replayed in stateless mode instead of synthesizing a message and
    /// are intentionally opaque to the agent. Assistant entries hold server
    /// output items; native tool-result entries hold `function_call_output`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_items: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_summary: Option<ToolActionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_metrics: Option<TurnMetrics>,
}

impl HistoryEntry {
    fn new(
        epoch: u64,
        revision: u64,
        sequence: u64,
        turn_id: TurnId,
        kind: HistoryKind,
        content: impl Into<String>,
        status: HistoryStatus,
    ) -> Self {
        let content = content.into();
        let approx_tokens = estimate_history_fields(&kind, &content);
        Self {
            epoch,
            revision,
            sequence,
            turn_id,
            kind,
            content,
            attachments: Vec::new(),
            status,
            approx_tokens,
            created_at: Utc::now(),
            api_items: Vec::new(),
            tool_summary: None,
            turn_metrics: None,
        }
    }

    #[must_use]
    pub const fn is_committed(&self) -> bool {
        matches!(self.status, HistoryStatus::Committed)
    }

    #[must_use]
    pub const fn is_request_context(&self) -> bool {
        matches!(
            self.status,
            HistoryStatus::Committed | HistoryStatus::Pending | HistoryStatus::Paused
        )
    }
}

pub(crate) fn compaction_notice_count(entry: &HistoryEntry) -> Option<usize> {
    if !matches!(entry.kind, HistoryKind::Assistant)
        || !matches!(entry.status, HistoryStatus::Superseded)
    {
        return None;
    }
    let (count, suffix) = entry.content.strip_prefix('[')?.split_once(' ')?;
    (suffix == "older history entries compacted into deterministic API-context summaries]")
        .then(|| count.parse().ok())
        .flatten()
}

pub(crate) fn is_context_capsule(entry: &HistoryEntry) -> bool {
    matches!(entry.kind, HistoryKind::Assistant)
        && entry
            .content
            .starts_with("[deterministic extractive context capsule;")
}

fn history_projection_differs(original: &HistoryEntry, projected: &HistoryEntry) -> bool {
    original.kind != projected.kind
        || original.content != projected.content
        || original.attachments != projected.attachments
        || original.api_items != projected.api_items
        || original.status != projected.status
        || original.tool_summary != projected.tool_summary
}

fn is_compacted_visible_entry(entry: &HistoryEntry) -> bool {
    entry.content.starts_with("[tool result compacted:")
        || entry
            .content
            .ends_with("\n[older draft truncated in persistent history]")
}

fn archive_visible_entry(archive: &mut Vec<HistoryEntry>, entry: &HistoryEntry) {
    if is_context_capsule(entry) || compaction_notice_count(entry).is_some() {
        return;
    }
    let mut archived = entry.clone();
    archived.api_items.clear();
    truncate_utf8_tail(&mut archived.content, MAX_TRANSCRIPT_ENTRY_BYTES);
    archived.approx_tokens = estimate_entry_tokens(&archived);
    match archive.binary_search_by_key(&archived.sequence, |candidate| candidate.sequence) {
        Ok(index) if archived.revision >= archive[index].revision => archive[index] = archived,
        Ok(_) => {}
        Err(index) => archive.insert(index, archived),
    }
    while archive.len() > MAX_TRANSCRIPT_ARCHIVE_ENTRIES
        || persistent_entry_bytes(archive) > MAX_TRANSCRIPT_ARCHIVE_BYTES
    {
        archive.remove(0);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentState {
    pub conversation_epoch: u64,
    pub history_revision: u64,
    pub history: Vec<HistoryEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    transcript_archive: Vec<HistoryEntry>,
    pub last_response_id: Option<String>,
    pub represented_through: u64,
    pub conversation_tokens: u64,
    pub last_reported_total_tokens: Option<u64>,
    /// Billed Responses API usage grouped by the exact Azure deployment name.
    /// Unlike context accounting, this ledger survives rewind so already-spent
    /// tokens cannot disappear from the cost panel.
    pub billing_usage: UsageLedger,
    /// Read-only side questions are persisted separately and never participate
    /// in the main Responses API replay unless the user explicitly promotes
    /// text into the ordinary composer.
    pub side_chat: SideChatState,
    /// Durable explicit Queue/Steer messages. They are kept outside ordinary
    /// history until the harness delivers them at their documented boundary.
    pub follow_ups: FollowUpState,
    /// Structured, immutable-diff-bound code-review reports and explicit
    /// per-finding decisions. This is session history, not API replay input.
    pub reviews: ReviewState,
    /// Session-scoped interactive modes. Serde default keeps older JSONL
    /// sessions loadable, while snapshots/forks persist the full mode state.
    pub work_modes: WorkModes,
    /// Context window selected for this session. Older journals omit it and
    /// inherit the current global default when first reopened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_context_budget: Option<u32>,
    /// Explicit session-scoped authority selected in the interactive approval
    /// center. Defaults remain fail-closed when loading older journals.
    pub auto_approval: AutoApprovalPolicy,
    /// One explicitly paused logical turn. Its pending history remains
    /// auditable and request-visible, but is never resumed without a click.
    pub paused_turn_id: Option<TurnId>,
    /// Persisted before work starts so an abrupt process exit can recover the
    /// logical turn as paused instead of silently treating it as complete.
    pub in_flight_turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pending_turn_metrics: BTreeMap<TurnId, TurnMetrics>,
    usage_reported_through: u64,
    next_sequence: u64,
}

impl AgentState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            conversation_epoch: 1,
            history_revision: 0,
            history: Vec::new(),
            transcript_archive: Vec::new(),
            last_response_id: None,
            represented_through: 0,
            conversation_tokens: 0,
            last_reported_total_tokens: None,
            billing_usage: UsageLedger::default(),
            side_chat: SideChatState::default(),
            follow_ups: FollowUpState::default(),
            reviews: ReviewState::default(),
            work_modes: WorkModes {
                plan: false,
                explore: false,
                review: false,
                deep_thinking: false,
                goal: None,
            },
            session_context_budget: None,
            auto_approval: AutoApprovalPolicy::default(),
            paused_turn_id: None,
            in_flight_turn_id: None,
            pending_turn_metrics: BTreeMap::new(),
            usage_reported_through: 0,
            next_sequence: 1,
        }
    }

    #[must_use]
    pub fn prompt_fits_budget(prompt: &str, context_budget: u32) -> bool {
        estimate_user_message_tokens(prompt) <= context_budget
    }

    /// Preflight an initial prompt before mutating history or issuing an API
    /// request. The first prompt is a semantic anchor and must never be
    /// truncated to force it under budget.
    pub fn validate_prompt_budget(
        prompt: &str,
        context_budget: u32,
    ) -> Result<(), ContextBudgetExceeded> {
        let required_tokens = estimate_user_message_tokens(prompt);
        if required_tokens <= context_budget {
            return Ok(());
        }
        Err(ContextBudgetExceeded {
            required_tokens,
            context_budget,
        })
    }

    /// Validate a new prompt together with the immutable first-user anchor.
    /// This prevents compaction from silently keeping the anchor while
    /// dropping the prompt that triggered the request.
    pub fn validate_next_prompt_budget(
        &self,
        prompt: &str,
        context_budget: u32,
    ) -> Result<(), ContextBudgetExceeded> {
        let anchor_tokens = self
            .history
            .iter()
            .find(|entry| entry.is_committed() && matches!(entry.kind, HistoryKind::User))
            .map_or(0, estimate_entry_tokens);
        let required_tokens = anchor_tokens.saturating_add(estimate_user_message_tokens(prompt));
        if required_tokens <= context_budget {
            return Ok(());
        }
        Err(ContextBudgetExceeded {
            required_tokens,
            context_budget,
        })
    }

    pub fn push_user(&mut self, turn_id: TurnId, content: impl Into<String>) -> u64 {
        self.push(
            turn_id,
            HistoryKind::User,
            content,
            HistoryStatus::Committed,
        )
    }

    /// Stage a user prompt for a logical turn. It is visible to request
    /// builders but excluded from durable committed history until
    /// `mark_turn_committed` succeeds.
    pub fn push_pending_user(&mut self, turn_id: TurnId, content: impl Into<String>) -> u64 {
        self.push(turn_id, HistoryKind::User, content, HistoryStatus::Pending)
    }

    pub fn push_pending_user_with_attachments(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
        attachments: Vec<AttachmentRef>,
    ) -> u64 {
        let sequence = self.push_pending_user(turn_id, content);
        if let Some(entry) = self
            .history
            .iter_mut()
            .find(|entry| entry.sequence == sequence)
        {
            entry.attachments = attachments;
            entry.approx_tokens = estimate_entry_tokens(entry);
        }
        sequence
    }

    pub fn push_assistant(&mut self, turn_id: TurnId, content: impl Into<String>) -> u64 {
        self.push(
            turn_id,
            HistoryKind::Assistant,
            content,
            HistoryStatus::Committed,
        )
    }

    pub fn push_assistant_with_api_items(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
        api_items: Vec<Value>,
    ) -> u64 {
        let sequence = self.push_assistant(turn_id, content);
        let _ = self.set_api_items(sequence, api_items);
        sequence
    }

    pub fn push_interrupted_assistant(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
    ) -> u64 {
        self.push(
            turn_id,
            HistoryKind::Assistant,
            content,
            HistoryStatus::Interrupted,
        )
    }

    pub fn push_superseded_assistant(
        &mut self,
        turn_id: TurnId,
        content: impl Into<String>,
    ) -> u64 {
        self.push(
            turn_id,
            HistoryKind::Assistant,
            content,
            HistoryStatus::Superseded,
        )
    }

    pub fn push_failed_assistant(&mut self, turn_id: TurnId, content: impl Into<String>) -> u64 {
        self.push(
            turn_id,
            HistoryKind::Assistant,
            content,
            HistoryStatus::Failed,
        )
    }

    pub fn push_tool_result(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        tool_name: impl Into<String>,
        outcome: &ToolOutcome,
    ) -> u64 {
        let (status, content) = match outcome {
            ToolOutcome::Success(output) => (ToolResultStatus::Success, output.clone()),
            ToolOutcome::Failure { message } => (ToolResultStatus::Failure, message.clone()),
            ToolOutcome::Declined { .. } => (
                ToolResultStatus::Declined,
                "The user declined this action.".to_owned(),
            ),
        };
        self.push_tool_diagnostic(turn_id, action_id, tool_name, status, content)
    }

    pub fn push_tool_result_with_action(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        action: &ToolAction,
        outcome: &ToolOutcome,
    ) -> u64 {
        let sequence = self.push_tool_result(turn_id, action_id, action.tool_name(), outcome);
        if let Some(index) = self
            .history
            .iter()
            .position(|entry| entry.sequence == sequence)
        {
            let revision = self.allocate_history_revision();
            let entry = &mut self.history[index];
            entry.tool_summary = Some(ToolActionSummary::from_action(action));
            entry.revision = revision;
        }
        sequence
    }

    pub fn push_tool_diagnostic(
        &mut self,
        turn_id: TurnId,
        action_id: ActionId,
        tool_name: impl Into<String>,
        outcome: ToolResultStatus,
        content: impl Into<String>,
    ) -> u64 {
        self.push(
            turn_id,
            HistoryKind::ToolResult {
                action_id,
                tool_name: tool_name.into(),
                outcome,
            },
            content,
            HistoryStatus::Committed,
        )
    }

    fn push(
        &mut self,
        turn_id: TurnId,
        kind: HistoryKind,
        content: impl Into<String>,
        status: HistoryStatus,
    ) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let revision = self.allocate_history_revision();
        self.history.push(HistoryEntry::new(
            self.conversation_epoch,
            revision,
            sequence,
            turn_id,
            kind,
            content,
            status,
        ));
        sequence
    }

    /// Attach lossless Responses API items to an existing history entry.
    pub fn set_api_items(&mut self, sequence: u64, api_items: Vec<Value>) -> bool {
        let Some(index) = self
            .history
            .iter()
            .position(|entry| entry.sequence == sequence)
        else {
            return false;
        };
        let revision = self.allocate_history_revision();
        let entry = &mut self.history[index];
        entry.api_items = api_items;
        entry.approx_tokens = estimate_entry_tokens(entry);
        entry.revision = revision;
        true
    }

    pub fn set_turn_metrics(&mut self, turn_id: TurnId, metrics: TurnMetrics) -> bool {
        let Some(index) = self.history.iter().rposition(|entry| {
            entry.turn_id == turn_id && matches!(entry.kind, HistoryKind::Assistant)
        }) else {
            return false;
        };
        let revision = self.allocate_history_revision();
        self.history[index].turn_metrics = Some(metrics);
        self.history[index].revision = revision;
        true
    }

    pub fn accumulate_turn_metrics(&mut self, turn_id: TurnId, segment: TurnMetrics) {
        self.pending_turn_metrics
            .entry(turn_id)
            .and_modify(|metrics| metrics.add_assign(&segment))
            .or_insert(segment);
    }

    pub fn complete_turn_metrics(
        &mut self,
        turn_id: TurnId,
        mut final_segment: TurnMetrics,
    ) -> bool {
        if let Some(mut accumulated) = self.pending_turn_metrics.remove(&turn_id) {
            accumulated.add_assign(&final_segment);
            final_segment = accumulated;
        }
        self.set_turn_metrics(turn_id, final_segment)
    }

    pub fn clear_turn_metrics(&mut self, turn_id: TurnId) {
        self.pending_turn_metrics.remove(&turn_id);
    }

    /// Commit only entries staged for this logical turn. Already committed
    /// causal history is never rewritten.
    pub fn mark_turn_committed(&mut self, turn_id: TurnId) -> usize {
        let transitioned = self.transition_uncommitted_turn(turn_id, HistoryStatus::Committed);
        if self.paused_turn_id == Some(turn_id) {
            self.paused_turn_id = None;
        }
        transitioned
    }

    pub fn mark_turn_failed(&mut self, turn_id: TurnId) -> usize {
        let transitioned = self.transition_uncommitted_turn(turn_id, HistoryStatus::Failed);
        if self.paused_turn_id == Some(turn_id) {
            self.paused_turn_id = None;
        }
        transitioned
    }

    pub fn mark_turn_cancelled(&mut self, turn_id: TurnId) -> usize {
        let transitioned = self.transition_uncommitted_turn(turn_id, HistoryStatus::Cancelled);
        if self.paused_turn_id == Some(turn_id) {
            self.paused_turn_id = None;
        }
        transitioned
    }

    pub fn mark_turn_paused(&mut self, turn_id: TurnId) -> usize {
        let transitioned =
            self.transition_exact_status(turn_id, HistoryStatus::Pending, HistoryStatus::Paused);
        if transitioned > 0 || self.history.iter().any(|entry| entry.turn_id == turn_id) {
            self.paused_turn_id = Some(turn_id);
        }
        transitioned
    }

    pub fn begin_turn(&mut self, turn_id: TurnId) {
        self.in_flight_turn_id = Some(turn_id);
    }

    pub fn finish_turn(&mut self, turn_id: TurnId) {
        if self.in_flight_turn_id == Some(turn_id) {
            self.in_flight_turn_id = None;
        }
    }

    pub fn resume_paused_turn(&mut self, turn_id: TurnId) -> usize {
        if self.paused_turn_id != Some(turn_id) {
            return 0;
        }
        let transitioned =
            self.transition_exact_status(turn_id, HistoryStatus::Paused, HistoryStatus::Pending);
        self.paused_turn_id = None;
        transitioned.max(1)
    }

    fn transition_uncommitted_turn(&mut self, turn_id: TurnId, status: HistoryStatus) -> usize {
        let mut transitioned = 0;
        let mut revision = self.history_revision;
        for entry in &mut self.history {
            if entry.turn_id == turn_id
                && matches!(entry.status, HistoryStatus::Pending | HistoryStatus::Paused)
            {
                entry.status = status.clone();
                revision = revision.saturating_add(1);
                entry.revision = revision;
                transitioned += 1;
            }
        }
        self.history_revision = revision;
        transitioned
    }

    fn transition_exact_status(
        &mut self,
        turn_id: TurnId,
        from: HistoryStatus,
        to: HistoryStatus,
    ) -> usize {
        let mut transitioned = 0;
        let mut revision = self.history_revision;
        for entry in &mut self.history {
            if entry.turn_id == turn_id && entry.status == from {
                entry.status = to.clone();
                revision = revision.saturating_add(1);
                entry.revision = revision;
                transitioned += 1;
            }
        }
        self.history_revision = revision;
        transitioned
    }

    pub fn mark_represented_through(&mut self, sequence: u64) {
        self.represented_through = self.represented_through.max(sequence);
    }

    #[must_use]
    pub fn last_committed_sequence(&self) -> u64 {
        self.history
            .iter()
            .rev()
            .find(|entry| entry.is_committed())
            .map_or(0, |entry| entry.sequence)
    }

    #[must_use]
    pub fn committed_after(&self, sequence: u64) -> Vec<HistoryEntry> {
        self.history
            .iter()
            .filter(|entry| entry.is_committed() && entry.sequence > sequence)
            .cloned()
            .collect()
    }

    /// Context for an in-flight request: durable history plus the pending
    /// prompt for the current logical turn. Failed/cancelled/superseded drafts
    /// are deliberately excluded from later submissions.
    #[must_use]
    pub fn request_context_after(&self, sequence: u64) -> Vec<HistoryEntry> {
        self.history
            .iter()
            .filter(|entry| entry.is_request_context() && entry.sequence > sequence)
            .cloned()
            .collect()
    }

    /// Return the exact unsent stateful input only when its canonical wire
    /// projection fits the local request budget. A stateful cursor must never
    /// advance after silently dropping an unsent ToolResult.
    pub fn checked_request_context_after(
        &self,
        sequence: u64,
        context_budget: u32,
    ) -> Result<Vec<HistoryEntry>, ContextBudgetExceeded> {
        let mut entries = self.request_context_after(sequence);
        for entry in &mut entries {
            entry.approx_tokens = estimate_entry_tokens(entry);
        }
        let required_tokens = u32::try_from(token_sum(&entries)).unwrap_or(u32::MAX);
        if required_tokens <= context_budget {
            return Ok(entries);
        }
        Err(ContextBudgetExceeded {
            required_tokens,
            context_budget,
        })
    }

    /// Compact API replay state while retaining the original transcript for UI and recovery.
    pub fn compact_persisted_history(
        &mut self,
        context_budget: u32,
        stateful: bool,
    ) -> Result<(), ContextBudgetExceeded> {
        self.compact_persisted_history_with_pressure(context_budget, stateful, true)
    }

    pub(crate) fn compact_persisted_history_for_exact_preflight(
        &mut self,
        context_budget: u32,
    ) -> Result<(), ContextBudgetExceeded> {
        self.compact_persisted_history_with_pressure(context_budget, false, false)
    }

    fn compact_persisted_history_with_pressure(
        &mut self,
        context_budget: u32,
        stateful: bool,
        use_reported_usage: bool,
    ) -> Result<(), ContextBudgetExceeded> {
        let projection = if stateful && self.last_response_id.is_some() {
            self.checked_request_context_after(self.represented_through, context_budget)?
        } else {
            self.checked_compacted_request_context_with_pressure(
                context_budget,
                use_reported_usage,
            )?
        };
        let projection = cap_causal_projection(projection, context_budget)?;
        let mut selected: BTreeMap<_, _> = projection
            .into_iter()
            .map(|entry| (entry.sequence, entry))
            .collect();
        let anchor_sequence = self
            .history
            .iter()
            .find(|entry| entry.is_committed() && matches!(entry.kind, HistoryKind::User))
            .map(|entry| entry.sequence);

        let mut retained = Vec::with_capacity(selected.len().saturating_add(MAX_PERSISTED_DRAFTS));
        let mut drafts = Vec::new();
        let mut omitted = 0_usize;
        let mut previous_notice = None;
        for mut entry in std::mem::take(&mut self.history) {
            if compaction_notice_count(&entry).is_some() {
                previous_notice = Some(entry);
                continue;
            }
            if let Some(projected) = selected.remove(&entry.sequence) {
                if history_projection_differs(&entry, &projected) {
                    archive_visible_entry(&mut self.transcript_archive, &entry);
                    omitted = omitted.saturating_add(1);
                }
                retained.push(projected);
            } else if stateful && Some(entry.sequence) == anchor_sequence {
                retained.push(entry);
            } else if entry.is_request_context() {
                archive_visible_entry(&mut self.transcript_archive, &entry);
                omitted = omitted.saturating_add(1);
            } else {
                archive_visible_entry(&mut self.transcript_archive, &entry);
                truncate_utf8_tail(&mut entry.content, MAX_PERSISTED_DRAFT_BYTES);
                entry.api_items.clear();
                entry.approx_tokens = estimate_entry_tokens(&entry);
                drafts.push(entry);
            }
        }

        if drafts.len() > MAX_PERSISTED_DRAFTS {
            let remove = drafts.len() - MAX_PERSISTED_DRAFTS;
            omitted = omitted.saturating_add(remove);
            drafts.drain(..remove);
        }
        retained.extend(drafts);
        let previous_omitted = previous_notice
            .as_ref()
            .and_then(compaction_notice_count)
            .unwrap_or(0);
        if omitted == 0
            && let Some(notice) = previous_notice
        {
            retained.push(notice);
        }
        retained.sort_by_key(|entry| entry.sequence);
        self.history = retained;
        if omitted > 0 {
            self.push_superseded_assistant(
                0,
                format!(
                    "[{} older history entries compacted into deterministic API-context summaries]",
                    previous_omitted.saturating_add(omitted)
                ),
            );
        }
        Ok(())
    }

    #[must_use]
    pub(crate) fn visible_history(&self) -> Vec<&HistoryEntry> {
        let mut visible = BTreeMap::<u64, &HistoryEntry>::new();
        for entry in &self.history {
            if !is_context_capsule(entry) {
                visible.insert(entry.sequence, entry);
            }
        }
        for archived in &self.transcript_archive {
            if is_context_capsule(archived) || compaction_notice_count(archived).is_some() {
                continue;
            }
            let replace = visible
                .get(&archived.sequence)
                .is_none_or(|active| archived.revision >= active.revision);
            if replace {
                visible.insert(archived.sequence, archived);
            }
        }
        visible.into_values().collect()
    }

    pub(crate) fn recover_legacy_visible_history(&mut self, previous: &Self) {
        if !self
            .history
            .iter()
            .any(|entry| is_context_capsule(entry) || compaction_notice_count(entry).is_some())
        {
            return;
        }
        let Some(last_sequence) = self
            .history
            .iter()
            .filter(|entry| compaction_notice_count(entry).is_none())
            .map(|entry| entry.sequence)
            .max()
        else {
            return;
        };
        let active = self
            .history
            .iter()
            .map(|entry| (entry.sequence, entry))
            .collect::<BTreeMap<_, _>>();
        let recovered = previous
            .visible_history()
            .into_iter()
            .filter(|entry| {
                entry.sequence <= last_sequence
                    && active.get(&entry.sequence).is_none_or(|current| {
                        is_context_capsule(current) || is_compacted_visible_entry(current)
                    })
            })
            .cloned()
            .collect::<Vec<_>>();
        for entry in recovered {
            archive_visible_entry(&mut self.transcript_archive, &entry);
        }
    }

    #[must_use]
    pub fn compacted_committed(&self, context_budget: u32) -> Vec<HistoryEntry> {
        self.compacted(context_budget, HistoryEntry::is_committed, true)
    }

    /// Budgeted request context, including a pending initial prompt.
    #[must_use]
    pub fn compacted_request_context(&self, context_budget: u32) -> Vec<HistoryEntry> {
        self.compacted(context_budget, HistoryEntry::is_request_context, true)
    }

    /// A request must never be emitted if strict compaction would discard the
    /// newest causal group. Sending an older anchor without the current prompt
    /// or ToolResult would silently change the conversation.
    pub fn checked_compacted_request_context(
        &self,
        context_budget: u32,
    ) -> Result<Vec<HistoryEntry>, ContextBudgetExceeded> {
        self.checked_compacted_request_context_with_pressure(context_budget, true)
    }

    fn checked_compacted_request_context_with_pressure(
        &self,
        context_budget: u32,
        use_reported_usage: bool,
    ) -> Result<Vec<HistoryEntry>, ContextBudgetExceeded> {
        let newest_required = self
            .history
            .iter()
            .rev()
            .find(|entry| entry.is_request_context())
            .map(|entry| entry.sequence);
        let compacted = self.compacted(
            context_budget,
            HistoryEntry::is_request_context,
            use_reported_usage,
        );
        if newest_required
            .is_none_or(|sequence| compacted.iter().any(|entry| entry.sequence == sequence))
        {
            return Ok(compacted);
        }
        Err(ContextBudgetExceeded {
            required_tokens: context_budget.saturating_add(1),
            context_budget,
        })
    }

    fn compacted(
        &self,
        context_budget: u32,
        include: impl Fn(&HistoryEntry) -> bool,
        use_reported_usage: bool,
    ) -> Vec<HistoryEntry> {
        let mut entries: Vec<_> = self
            .history
            .iter()
            .filter(|entry| include(entry))
            .cloned()
            .collect();
        // `HistoryEntry` is serde-public, so never trust a deserialized or
        // externally-mutated token estimate when enforcing a hard limit.
        for entry in &mut entries {
            entry.approx_tokens = estimate_entry_tokens(entry);
        }

        let budget = u64::from(context_budget);
        let reported_projection = use_reported_usage
            .then(|| {
                self.last_reported_total_tokens.map(|reported| {
                    let unreported = entries
                        .iter()
                        .filter(|entry| entry.sequence > self.usage_reported_through)
                        .map(|entry| u64::from(entry.approx_tokens))
                        .sum::<u64>();
                    reported.saturating_add(unreported)
                })
            })
            .flatten();
        // Server usage is the preferred compaction trigger, but it is not a
        // serialized-size bound. Opaque encrypted/reasoning items can be much
        // larger than their reported token contribution, so the hard local
        // postcondition must also consider the recomputed replay estimate.
        let estimated_tokens = token_sum(&entries);
        let projected_tokens =
            reported_projection.map_or(estimated_tokens, |reported| reported.max(estimated_tokens));
        if projected_tokens <= budget {
            return entries;
        }
        // Authoritative usage remains compaction pressure after the trigger.
        // Scale the deterministic wire budget when the server reports more
        // tokens than our byte-based estimate; otherwise hundreds of small
        // messages could be returned unchanged forever despite an over-budget
        // server context.
        let effective_budget = if projected_tokens > estimated_tokens && projected_tokens > 0 {
            budget.saturating_mul(estimated_tokens) / projected_tokens
        } else {
            budget
        };

        for entry in &mut entries {
            compact_tool_result(entry);
        }
        if token_sum(&entries) <= effective_budget {
            return entries;
        }

        let first_user_index = entries
            .iter()
            .position(|entry| matches!(entry.kind, HistoryKind::User));

        let mut keep = Vec::new();
        let suffix_start = if let Some(index) = first_user_index {
            let anchor = entries[index].clone();
            if u64::from(anchor.approx_tokens) > effective_budget {
                // The public preflight API makes this an explicit caller
                // error. Returning no projection is fail-closed and, unlike a
                // placeholder, never changes the prompt's meaning.
                return Vec::new();
            }
            keep.push(anchor);
            index + 1
        } else {
            0
        };

        // One user turn can contain many API/tool rounds. Treating the whole
        // turn as indivisible makes a long autonomous tool loop impossible to
        // compact. Each assistant tool request plus its results is a causal
        // unit; a final assistant answer remains attached to the preceding
        // unit, while user messages form explicit semantic anchors.
        let groups = causal_replay_groups(&entries[suffix_start..]);
        let latest_user_sequence = entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.kind, HistoryKind::User))
            .map(|entry| entry.sequence);
        let mut selected = vec![false; groups.len()];
        let mut used = token_sum(&keep);
        for (index, group) in groups.iter().enumerate() {
            if latest_user_sequence
                .is_some_and(|sequence| group.iter().any(|entry| entry.sequence == sequence))
            {
                let cost = token_sum(group);
                if used.saturating_add(cost) > effective_budget {
                    return Vec::new();
                }
                used = used.saturating_add(cost);
                selected[index] = true;
            }
        }
        let group_count = groups.len();
        for index in (0..groups.len()).rev() {
            if selected[index] {
                continue;
            }
            let group = &groups[index];
            let cost = token_sum(group);
            if used.saturating_add(cost) > effective_budget {
                break;
            }
            used = used.saturating_add(cost);
            selected[index] = true;
        }
        let omitted_groups =
            group_count.saturating_sub(selected.iter().filter(|is_selected| **is_selected).count());
        let omitted_entries = groups
            .iter()
            .zip(&selected)
            .filter(|(_, is_selected)| !**is_selected)
            .flat_map(|(group, _)| group.iter().cloned())
            .collect::<Vec<_>>();
        keep.extend(
            groups
                .into_iter()
                .zip(&selected)
                .filter(|(_, is_selected)| **is_selected)
                .flat_map(|(group, _)| group),
        );
        if omitted_groups > 0 {
            let remaining = effective_budget.saturating_sub(used);
            let capsule_budget = remaining
                .min(effective_budget / 8)
                .min(MAX_CONTEXT_CAPSULE_TOKENS);
            if let Some(capsule) =
                build_context_capsule(&omitted_entries, omitted_groups, capsule_budget)
            {
                used = used.saturating_add(u64::from(capsule.approx_tokens));
                keep.push(capsule);
            }
        }
        keep.sort_by_key(|entry| entry.sequence);
        debug_assert!(used <= effective_budget);
        debug_assert!(token_sum(&keep) <= effective_budget);
        keep
    }

    /// Build a lossless stateless request sequence. Known history entries are
    /// serialized as EasyInputMessages; entries with attached API items replay
    /// those opaque items in their original order instead.
    #[must_use]
    pub fn stateless_replay_input(&self, context_budget: u32) -> Vec<Value> {
        self.stateless_replay_input_from(self.compacted_request_context(context_budget))
    }

    pub fn checked_stateless_replay_input(
        &self,
        context_budget: u32,
    ) -> Result<Vec<Value>, ContextBudgetExceeded> {
        self.checked_compacted_request_context(context_budget)
            .map(|entries| self.stateless_replay_input_from(entries))
    }

    pub(crate) fn checked_stateless_replay_input_for_exact_preflight(
        &self,
        context_budget: u32,
    ) -> Result<Vec<Value>, ContextBudgetExceeded> {
        self.checked_compacted_request_context_with_pressure(context_budget, false)
            .map(|entries| self.stateless_replay_input_from(entries))
    }

    fn stateless_replay_input_from(&self, entries: Vec<HistoryEntry>) -> Vec<Value> {
        repaired_replay_items(entries)
    }

    pub fn add_usage(&mut self, total_tokens: u64) {
        self.conversation_tokens = self.conversation_tokens.saturating_add(total_tokens);
        self.last_reported_total_tokens = Some(total_tokens);
        self.usage_reported_through = self.last_committed_sequence();
    }

    pub fn record_usage(&mut self, total_tokens: u64, represented_through: u64) {
        self.conversation_tokens = self.conversation_tokens.saturating_add(total_tokens);
        self.last_reported_total_tokens = Some(total_tokens);
        self.usage_reported_through = represented_through;
    }

    pub fn record_deployment_usage(
        &mut self,
        deployment: &str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        represented_through: u64,
    ) {
        self.record_usage(total_tokens, represented_through);
        self.billing_usage.record(
            deployment,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            total_tokens,
        );
    }

    pub fn reset_billing_usage(&mut self) {
        self.billing_usage.clear();
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.transcript_archive.clear();
        self.conversation_epoch = self.conversation_epoch.saturating_add(1);
        self.history_revision = 0;
        self.last_response_id = None;
        self.represented_through = 0;
        self.conversation_tokens = 0;
        self.last_reported_total_tokens = None;
        self.billing_usage.clear();
        self.side_chat.clear();
        self.follow_ups.clear();
        self.reviews.clear();
        self.paused_turn_id = None;
        self.in_flight_turn_id = None;
        self.pending_turn_metrics.clear();
        self.usage_reported_through = 0;
    }

    pub(crate) fn advance_conversation_epoch_past(&mut self, previous_epoch: u64) {
        let next_epoch = self
            .conversation_epoch
            .max(previous_epoch)
            .saturating_add(1);
        self.conversation_epoch = next_epoch;
        for entry in &mut self.history {
            entry.epoch = next_epoch;
        }
        for entry in &mut self.transcript_archive {
            entry.epoch = next_epoch;
        }
        self.history_revision = self
            .history_revision
            .max(
                self.history
                    .iter()
                    .chain(&self.transcript_archive)
                    .map(|entry| entry.revision)
                    .max()
                    .unwrap_or(0),
            )
            .saturating_add(1);
    }

    /// Restore a previously captured conversation boundary while minting a
    /// fresh epoch. The old server response chain is deliberately discarded:
    /// after a rewind the next request must be reconstructed from the restored
    /// local history instead of accidentally continuing the abandoned branch.
    pub fn restore_checkpoint(&mut self, checkpoint: &Self) {
        let next_epoch = self.conversation_epoch.saturating_add(1);
        let billing_usage = self.billing_usage.clone();
        let side_chat = self.side_chat.clone();
        let follow_ups = self.follow_ups.clone();
        let session_context_budget = self.session_context_budget;
        *self = checkpoint.clone();
        self.billing_usage = billing_usage;
        self.side_chat = side_chat;
        self.follow_ups = follow_ups;
        self.session_context_budget = session_context_budget;
        self.conversation_epoch = next_epoch;
        self.history_revision = self.history_revision.saturating_add(1);
        self.last_response_id = None;
        self.represented_through = 0;
        self.usage_reported_through = 0;
        self.next_sequence = self
            .history
            .iter()
            .chain(&self.transcript_archive)
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        for entry in &mut self.history {
            entry.epoch = next_epoch;
        }
        for entry in &mut self.transcript_archive {
            entry.epoch = next_epoch;
        }
        if self
            .paused_turn_id
            .is_some_and(|turn_id| !self.history.iter().any(|entry| entry.turn_id == turn_id))
        {
            self.paused_turn_id = None;
        }
    }

    /// Normalize a state loaded after process termination. A prompt that was
    /// merely pending when the process stopped is retained for auditability
    /// but cannot be replayed as if the server had never seen it.
    pub fn recover_after_restart(&mut self) {
        if let Some(turn_id) = self.in_flight_turn_id.take() {
            self.mark_turn_paused(turn_id);
        }
        let next_epoch = self.conversation_epoch.saturating_add(1);
        self.conversation_epoch = next_epoch;
        self.last_response_id = None;
        self.represented_through = 0;
        self.usage_reported_through = 0;
        self.history_revision = self
            .history
            .iter()
            .chain(&self.transcript_archive)
            .map(|entry| entry.revision)
            .max()
            .unwrap_or(0)
            .max(self.history_revision);
        let resumable_turn = self.paused_turn_id;
        for entry in &mut self.history {
            entry.epoch = next_epoch;
            if matches!(entry.status, HistoryStatus::Pending)
                || matches!(entry.status, HistoryStatus::Paused)
                    && Some(entry.turn_id) != resumable_turn
            {
                self.history_revision = self.history_revision.saturating_add(1);
                entry.revision = self.history_revision;
                entry.status = HistoryStatus::Cancelled;
            }
        }
        for entry in &mut self.transcript_archive {
            entry.epoch = next_epoch;
        }
        if self
            .paused_turn_id
            .is_some_and(|turn_id| !self.history.iter().any(|entry| entry.turn_id == turn_id))
        {
            self.paused_turn_id = None;
        }
        self.pending_turn_metrics
            .retain(|turn_id, _| Some(*turn_id) == self.paused_turn_id);
        self.side_chat.recover_after_restart();
        self.follow_ups.recover_after_restart();
        self.history_revision = self.history_revision.saturating_add(1);
        self.next_sequence = self
            .history
            .iter()
            .chain(&self.transcript_archive)
            .map(|entry| entry.sequence)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
    }

    fn allocate_history_revision(&mut self) -> u64 {
        self.history_revision = self.history_revision.saturating_add(1);
        self.history_revision
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

const fn default_history_epoch() -> u64 {
    1
}

const SERIALIZED_ITEM_RESERVE_TOKENS: u32 = 4;

#[must_use]
fn estimate_serialized_tokens(encoded_bytes: usize) -> u32 {
    u32::try_from(encoded_bytes)
        .unwrap_or(u32::MAX)
        .saturating_add(3)
        .saturating_div(4)
        .saturating_add(SERIALIZED_ITEM_RESERVE_TOKENS)
}

#[must_use]
fn estimate_history_fields(kind: &HistoryKind, content: &str) -> u32 {
    let wire = match kind {
        HistoryKind::User => serde_json::json!({"role": "user", "content": content}),
        HistoryKind::Assistant => {
            serde_json::json!({"role": "assistant", "content": content})
        }
        HistoryKind::ToolResult {
            action_id,
            tool_name,
            outcome,
        } => {
            let status = match outcome {
                ToolResultStatus::Success => "success",
                ToolResultStatus::Failure => "failure",
                ToolResultStatus::Declined => "declined",
                ToolResultStatus::ParseError => "parse_error",
            };
            serde_json::json!({
                "role": "user",
                "content": serde_json::json!({
                    "type": "tool_result",
                    "action_id": action_id,
                    "tool": tool_name,
                    "status": status,
                    "content": content,
                }).to_string(),
            })
        }
    };
    serde_json::to_vec(&wire).map_or(u32::MAX, |encoded| {
        estimate_serialized_tokens(encoded.len())
    })
}

#[must_use]
fn estimate_user_message_tokens(content: &str) -> u32 {
    estimate_history_fields(&HistoryKind::User, content)
}

#[must_use]
fn estimate_entry_tokens(entry: &HistoryEntry) -> u32 {
    let mut estimate = if entry.api_items.is_empty() {
        estimate_history_fields(&entry.kind, &entry.content)
    } else {
        let bytes = serde_json::to_vec(&entry.api_items)
            .map(|encoded| encoded.len())
            .unwrap_or(usize::MAX);
        let mut tokens = estimate_serialized_tokens(bytes);
        if matches!(entry.kind, HistoryKind::ToolResult { .. })
            && !entry.api_items.iter().any(is_function_call_output)
        {
            tokens = tokens.saturating_add(estimate_history_fields(&entry.kind, &entry.content));
        }
        tokens
    };
    estimate = estimate.saturating_add(attachment_tokens(&entry.attachments));
    estimate
}

fn attachment_tokens(attachments: &[AttachmentRef]) -> u32 {
    attachments.iter().fold(0_u32, |total, attachment| {
        let estimated = match attachment.kind {
            crate::attachments::AttachmentKind::Image => 4_096,
            crate::attachments::AttachmentKind::Document
            | crate::attachments::AttachmentKind::Text => {
                u32::try_from(attachment.size_bytes.saturating_add(3) / 4)
                    .unwrap_or(u32::MAX)
                    .min(100_000)
            }
            crate::attachments::AttachmentKind::Audio
            | crate::attachments::AttachmentKind::Video => 16_384,
        };
        total.saturating_add(estimated)
    })
}

#[must_use]
fn token_sum(entries: &[HistoryEntry]) -> u64 {
    entries
        .iter()
        .map(|entry| u64::from(entry.approx_tokens))
        .sum()
}

/// Convert persisted history to canonical Responses input while repairing a
/// legacy DEcode journal bug that discarded `function_call_output` items
/// during tool-result compaction. The repair is projection-only: the audit
/// journal remains untouched, and the original call ID is paired with the
/// corresponding ordered tool-result content.
pub(crate) fn repaired_replay_items(entries: Vec<HistoryEntry>) -> Vec<Value> {
    let mut replay = Vec::new();
    let mut pending_calls = VecDeque::new();
    for entry in entries {
        let is_tool_result = matches!(entry.kind, HistoryKind::ToolResult { .. });
        if !is_tool_result && !pending_calls.is_empty() {
            flush_unresolved_calls(&mut replay, &mut pending_calls);
        }

        if is_tool_result
            && !entry.api_items.iter().any(is_function_call_output)
            && let Some(call_id) = pending_calls.pop_front()
        {
            replay.push(function_call_output(call_id, entry.content));
            // A legacy tool-result entry may still carry unrelated opaque
            // metadata. Preserve it after the repaired causal output.
            replay.extend(entry.api_items);
            continue;
        }

        if entry.api_items.is_empty() {
            replay.push(history_entry_message(&entry));
            continue;
        }
        for item in entry.api_items {
            match item.get("type").and_then(Value::as_str) {
                Some("function_call") => {
                    if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
                        pending_calls.push_back(call_id.to_owned());
                    }
                }
                Some("function_call_output") => {
                    if let Some(call_id) = item.get("call_id").and_then(Value::as_str)
                        && let Some(index) =
                            pending_calls.iter().position(|pending| pending == call_id)
                    {
                        let _ = pending_calls.remove(index);
                    }
                }
                _ => {}
            }
            replay.push(item);
        }
    }
    flush_unresolved_calls(&mut replay, &mut pending_calls);
    replay
}

fn is_function_call_output(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call_output")
}

fn function_call_output(call_id: String, output: String) -> Value {
    serde_json::json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

fn flush_unresolved_calls(replay: &mut Vec<Value>, pending_calls: &mut VecDeque<String>) {
    for call_id in pending_calls.drain(..) {
        replay.push(function_call_output(
            call_id,
            "Legacy session recovery: the corresponding tool outcome was not persisted. Rerun the tool before relying on its result."
                .to_owned(),
        ));
    }
}

fn compact_tool_result(entry: &mut HistoryEntry) {
    let HistoryKind::ToolResult {
        action_id,
        tool_name,
        ..
    } = &entry.kind
    else {
        return;
    };
    if entry.content.len() <= 512 {
        return;
    }
    let bytes = entry.content.len();
    let digest = sha256_hex(entry.content.as_bytes());
    let path = entry
        .tool_summary
        .as_ref()
        .and_then(|summary| summary.target.as_deref())
        .unwrap_or("-");
    let compacted = format!(
        "[tool result compacted: tool={tool_name}, action_id={action_id}, path={path}, bytes={bytes}, sha256={digest}; rerun the tool if exact bytes are needed]"
    );
    entry.content.clone_from(&compacted);
    // Native Responses replay requires the original call_id. Keep the
    // function_call_output item and replace only its large output payload;
    // clearing api_items would turn a valid call/result pair into malformed
    // stateless history.
    for item in &mut entry.api_items {
        if item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && let Some(object) = item.as_object_mut()
        {
            object.insert("output".to_owned(), Value::String(compacted.clone()));
        }
    }
    entry.approx_tokens = estimate_entry_tokens(entry);
}

fn causal_replay_groups(entries: &[HistoryEntry]) -> Vec<Vec<HistoryEntry>> {
    let mut groups: Vec<Vec<HistoryEntry>> = Vec::new();
    for (index, entry) in entries.iter().cloned().enumerate() {
        let next_is_tool_result = entries.get(index.saturating_add(1)).is_some_and(|next| {
            next.turn_id == entry.turn_id && matches!(next.kind, HistoryKind::ToolResult { .. })
        });
        let starts_group = match entry.kind {
            HistoryKind::User => true,
            HistoryKind::Assistant => {
                next_is_tool_result
                    || groups.last().is_none_or(|group| {
                        group
                            .last()
                            .is_none_or(|previous| previous.turn_id != entry.turn_id)
                    })
            }
            HistoryKind::ToolResult { .. } => groups.last().is_none_or(|group| {
                group
                    .last()
                    .is_none_or(|previous| previous.turn_id != entry.turn_id)
            }),
        };
        if starts_group {
            groups.push(vec![entry]);
        } else if let Some(group) = groups.last_mut() {
            group.push(entry);
        }
    }
    groups
}

fn cap_causal_projection(
    entries: Vec<HistoryEntry>,
    context_budget: u32,
) -> Result<Vec<HistoryEntry>, ContextBudgetExceeded> {
    if entries.len() <= MAX_PERSISTED_API_ENTRIES
        && persistent_entry_bytes(&entries) <= MAX_PERSISTED_API_BYTES
    {
        return Ok(entries);
    }
    let first_user_index = entries
        .iter()
        .position(|entry| matches!(entry.kind, HistoryKind::User));
    let (anchor, suffix_start) = first_user_index.map_or((None, 0), |index| {
        (Some(entries[index].clone()), index.saturating_add(1))
    });
    let groups = causal_replay_groups(&entries[suffix_start..]);

    let mut kept = anchor.into_iter().collect::<Vec<_>>();
    let mut used_entries = kept.len();
    let mut used_bytes = persistent_entry_bytes(&kept);
    if used_entries > MAX_PERSISTED_API_ENTRIES || used_bytes > MAX_PERSISTED_API_BYTES {
        return Err(ContextBudgetExceeded {
            required_tokens: context_budget.saturating_add(1),
            context_budget,
        });
    }
    let mut suffix = Vec::new();
    for group in groups.into_iter().rev() {
        let group_bytes = persistent_entry_bytes(&group);
        if used_entries.saturating_add(group.len()) > MAX_PERSISTED_API_ENTRIES
            || used_bytes.saturating_add(group_bytes) > MAX_PERSISTED_API_BYTES
        {
            if suffix.is_empty() {
                return Err(ContextBudgetExceeded {
                    required_tokens: context_budget.saturating_add(1),
                    context_budget,
                });
            }
            break;
        }
        used_entries = used_entries.saturating_add(group.len());
        used_bytes = used_bytes.saturating_add(group_bytes);
        suffix.push(group);
    }
    suffix.reverse();
    kept.extend(suffix.into_iter().flatten());
    Ok(kept)
}

fn persistent_entry_bytes(entries: &[HistoryEntry]) -> usize {
    entries.iter().fold(0_usize, |total, entry| {
        let api_bytes =
            serde_json::to_vec(&entry.api_items).map_or(usize::MAX, |value| value.len());
        total
            .saturating_add(entry.content.len())
            .saturating_add(api_bytes)
            .saturating_add(256)
    })
}

fn truncate_utf8_tail(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let suffix = "\n[older draft truncated in persistent history]";
    let mut end = max_bytes.saturating_sub(suffix.len()).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value.truncate(end);
    value.push_str(suffix);
}

#[must_use]
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn build_context_capsule(
    entries: &[HistoryEntry],
    omitted_groups: usize,
    token_budget: u64,
) -> Option<HistoryEntry> {
    if token_budget < 32 || omitted_groups == 0 {
        return None;
    }
    let mut groups: Vec<&[HistoryEntry]> = Vec::new();
    let mut start = 0;
    while start < entries.len() && groups.len() < omitted_groups {
        let turn_id = entries[start].turn_id;
        let mut end = start + 1;
        while end < entries.len() && entries[end].turn_id == turn_id {
            end += 1;
        }
        groups.push(&entries[start..end]);
        start = end;
    }
    if groups.is_empty() {
        return None;
    }

    let header = "[deterministic extractive context capsule; whitespace-normalized excerpts; no generated claims; full turns remain in session history]";
    let mut selected = Vec::new();
    for group in groups.into_iter().rev().take(MAX_CONTEXT_CAPSULE_TURNS) {
        let line = summarize_context_group(group);
        let mut candidate_lines = selected.clone();
        candidate_lines.push(line);
        candidate_lines.reverse();
        let candidate_content = format!("{header}\n{}", candidate_lines.join("\n"));
        let mut candidate = HistoryEntry::new(
            group.first().map_or(1, |entry| entry.epoch),
            group.first().map_or(0, |entry| entry.revision),
            group.first().map_or(0, |entry| entry.sequence),
            0,
            HistoryKind::Assistant,
            candidate_content,
            HistoryStatus::Committed,
        );
        candidate.approx_tokens = estimate_entry_tokens(&candidate);
        if u64::from(candidate.approx_tokens) > token_budget {
            break;
        }
        candidate_lines.reverse();
        selected = candidate_lines;
    }
    if selected.is_empty() {
        return None;
    }
    selected.reverse();
    let content = format!("{header}\n{}", selected.join("\n"));
    let first = entries.first()?;
    let mut capsule = HistoryEntry::new(
        first.epoch,
        first.revision,
        first.sequence,
        0,
        HistoryKind::Assistant,
        content,
        HistoryStatus::Committed,
    );
    capsule.approx_tokens = estimate_entry_tokens(&capsule);
    (u64::from(capsule.approx_tokens) <= token_budget).then_some(capsule)
}

fn summarize_context_group(group: &[HistoryEntry]) -> String {
    let turn_id = group.first().map_or(0, |entry| entry.turn_id);
    let user = group
        .iter()
        .find(|entry| matches!(entry.kind, HistoryKind::User))
        .map(|entry| context_excerpt(&entry.content))
        .unwrap_or_else(|| "-".to_owned());
    let assistant = group
        .iter()
        .rev()
        .find(|entry| matches!(entry.kind, HistoryKind::Assistant))
        .map(|entry| context_excerpt(&entry.content))
        .unwrap_or_else(|| "-".to_owned());
    let tools = group
        .iter()
        .filter_map(|entry| entry.tool_summary.as_ref())
        .take(8)
        .map(|summary| {
            summary.target.as_ref().map_or_else(
                || summary.tool_name.clone(),
                |target| format!("{}:{}", summary.tool_name, context_excerpt(target)),
            )
        })
        .collect::<Vec<_>>();
    let mut digest = Sha256::new();
    for entry in group {
        digest.update(entry.sequence.to_le_bytes());
        digest.update(entry.content.as_bytes());
    }
    let digest = digest.finalize();
    let mut digest_hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(digest_hex, "{byte:02x}");
    }
    let user = serde_json::to_string(&user).unwrap_or_else(|_| "\"-\"".to_owned());
    let assistant = serde_json::to_string(&assistant).unwrap_or_else(|_| "\"-\"".to_owned());
    format!(
        "- turn={turn_id}; user_excerpt={user}; assistant_excerpt={assistant}; tools=[{}]; sha256={digest_hex}",
        tools.join(","),
    )
}

fn context_excerpt(value: &str) -> String {
    use std::collections::VecDeque;

    let normalized = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.chars().count() <= MAX_CONTEXT_CAPSULE_EXCERPT_CHARS {
        return normalized;
    }

    // Preserve both the original intent and the newest outcome. Keeping only
    // a prefix silently loses final verification results and changed decisions.
    const ELLIPSIS: &str = " … ";
    let ellipsis_chars = ELLIPSIS.chars().count();
    let available = MAX_CONTEXT_CAPSULE_EXCERPT_CHARS.saturating_sub(ellipsis_chars);
    let head_limit = available.saturating_mul(3) / 5;
    let tail_limit = available.saturating_sub(head_limit);
    let head = normalized.chars().take(head_limit).collect::<String>();
    let mut tail = VecDeque::with_capacity(tail_limit);
    for character in normalized.chars().rev().take(tail_limit) {
        tail.push_front(character);
    }
    let tail = tail.into_iter().collect::<String>();
    format!("{}{}{}", head.trim_end(), ELLIPSIS, tail.trim_start())
}

pub(crate) fn history_entry_message(entry: &HistoryEntry) -> Value {
    let (role, content) = match &entry.kind {
        HistoryKind::User => ("user", entry.content.clone()),
        HistoryKind::Assistant => ("assistant", entry.content.clone()),
        HistoryKind::ToolResult {
            action_id,
            tool_name,
            outcome,
        } => {
            let status = match outcome {
                ToolResultStatus::Success => "success",
                ToolResultStatus::Failure => "failure",
                ToolResultStatus::Declined => "declined",
                ToolResultStatus::ParseError => "parse_error",
            };
            (
                "user",
                serde_json::json!({
                    "type": "tool_result",
                    "action_id": action_id,
                    "tool": tool_name,
                    "status": status,
                    "content": entry.content,
                })
                .to_string(),
            )
        }
    };
    if matches!(entry.kind, HistoryKind::User) && !entry.attachments.is_empty() {
        let mut parts = Vec::with_capacity(entry.attachments.len().saturating_add(1));
        if !content.trim().is_empty() {
            parts.push(serde_json::json!({"type": "input_text", "text": content}));
        }
        parts.extend(
            entry
                .attachments
                .iter()
                .map(AttachmentRef::placeholder_part),
        );
        serde_json::json!({"role": role, "content": parts})
    } else {
        serde_json::json!({"role": role, "content": content})
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{
        AgentState, ContextBudgetExceeded, HistoryKind, HistoryStatus,
        MAX_CONTEXT_CAPSULE_EXCERPT_CHARS, ToolResultStatus, TurnMetrics, build_context_capsule,
        context_excerpt, token_sum,
    };
    use crate::{
        api::ReasoningEffort,
        attachments::{AttachmentDetail, AttachmentKind, AttachmentRef},
        parser::{ToolAction, ToolOutcome},
    };

    #[test]
    fn checkpoint_restore_mints_epoch_and_drops_abandoned_branch()
    -> Result<(), crate::agent::SideChatError> {
        let mut state = AgentState::new();
        state.session_context_budget = Some(300_000);
        state.push_user(1, "anchor");
        state.last_response_id = Some("response-old".to_owned());
        state.represented_through = 1;
        let checkpoint = state.clone();
        state.session_context_budget = Some(200_000);
        state.push_user(2, "abandoned");
        state.record_deployment_usage("coding-prod", 10, 3, 20, 30, 2);
        let side = state.side_chat.start(
            "why is this safe?".to_owned(),
            state.history_revision,
            "review-model".to_owned(),
            ReasoningEffort::High,
        )?;
        state
            .side_chat
            .complete(side.id, "provisional answer".to_owned(), 10, 2, 4, 14)?;

        state.restore_checkpoint(&checkpoint);

        assert_eq!(state.history.len(), 1);
        assert_eq!(state.history[0].content, "anchor");
        assert_eq!(state.conversation_epoch, 2);
        assert!(state.last_response_id.is_none());
        assert_eq!(state.represented_through, 0);
        assert_eq!(state.session_context_budget, Some(200_000));
        let usage = crate::usage::PricingCatalog::default()
            .snapshot(&state.billing_usage, state.last_reported_total_tokens);
        assert_eq!(usage.usage.total_tokens, 30);
        assert_eq!(state.side_chat.exchanges().len(), 1);
        assert_eq!(state.side_chat.exchanges()[0].answer, "provisional answer");
        Ok::<(), crate::agent::SideChatError>(())
    }

    #[test]
    fn interrupted_drafts_never_enter_committed_context() {
        let mut state = AgentState::new();
        state.push_user(1, "request");
        state.push_interrupted_assistant(1, "partial");
        state.push_assistant(1, "complete");

        let committed = state.committed_after(0);
        assert_eq!(committed.len(), 2);
        assert!(
            committed
                .iter()
                .all(|entry| { matches!(entry.status, HistoryStatus::Committed) })
        );
    }

    #[test]
    fn compaction_is_reproducible_and_preserves_first_prompt() {
        let mut state = AgentState::new();
        state.push_user(1, "first prompt");
        state.push_tool_result(1, 1, "read_file", &ToolOutcome::success("x".repeat(4_096)));
        state.push_assistant(1, "decision");
        state.push_user(2, "newest prompt");
        state.push_assistant(2, "newest decision");

        let first = state.compacted_committed(320);
        let second = state.compacted_committed(320);
        assert_eq!(first, second);
        assert!(first.iter().any(|entry| {
            matches!(entry.kind, HistoryKind::User) && entry.content == "first prompt"
        }));
        assert!(
            first
                .iter()
                .any(|entry| { entry.content.contains("sha256=") })
        );
    }

    #[test]
    fn persisted_compaction_notice_is_visible_at_the_newest_edge()
    -> Result<(), ContextBudgetExceeded> {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        for turn_id in 2..=20 {
            state.push_user(turn_id, format!("historical request {turn_id}"));
            state.push_assistant(turn_id, format!("historical answer {turn_id}"));
        }

        state.compact_persisted_history(64, false)?;

        let notice = state.history.last().ok_or(ContextBudgetExceeded {
            required_tokens: 1,
            context_budget: 0,
        })?;
        assert!(notice.content.contains("history entries compacted"));
        assert!(matches!(notice.status, HistoryStatus::Superseded));
        Ok(())
    }

    #[test]
    fn persistent_compaction_never_discards_visible_chat_history()
    -> Result<(), ContextBudgetExceeded> {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        for turn_id in 2..=20 {
            state.push_user(turn_id, format!("historical request {turn_id}"));
            state.push_assistant(turn_id, format!("historical answer {turn_id}"));
        }
        let original = state.history.clone();

        state.compact_persisted_history(64, false)?;

        let visible = state.visible_history();
        for expected in original {
            assert!(visible.iter().any(|entry| {
                entry.sequence == expected.sequence
                    && entry.kind == expected.kind
                    && entry.content == expected.content
            }));
        }
        Ok(())
    }

    #[test]
    fn persisted_compaction_notice_covers_reduced_tool_results() -> Result<(), ContextBudgetExceeded>
    {
        let mut state = AgentState::new();
        state.push_user(1, "inspect the output");
        state.push_tool_result(1, 1, "read_file", &ToolOutcome::success("x".repeat(4_096)));

        state.compact_persisted_history(256, false)?;

        assert!(state.history.iter().any(|entry| {
            super::compaction_notice_count(entry).is_some_and(|compacted| compacted >= 1)
        }));
        Ok(())
    }

    #[test]
    fn extractive_context_capsule_keeps_intent_outcome_paths_and_digest() -> Result<(), &'static str>
    {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_user(2, "Preserve the authentication invariant exactly");
        state.push_tool_result_with_action(
            2,
            7,
            &ToolAction::ReadFile {
                path: "src/auth.rs".to_owned(),
            },
            &ToolOutcome::success("large historical output".repeat(80)),
        );
        state.push_assistant(2, "Implemented the bounded token verification path");
        state.push_user(3, "Add recovery without changing the public contract");
        state.push_assistant(3, "Recovery now resumes only from a safe boundary");

        let capsule = build_context_capsule(&state.history[1..], 2, 512)
            .ok_or("capsule should fit the explicit test budget")?;
        assert!(capsule.content.contains("authentication invariant"));
        assert!(capsule.content.contains("safe boundary"));
        assert!(capsule.content.contains("read_file:src/auth.rs"));
        assert!(capsule.content.contains("sha256="));
        assert!(capsule.api_items.is_empty());
        assert!(u64::from(capsule.approx_tokens) <= 512);
        Ok(())
    }

    #[test]
    fn context_excerpt_keeps_initial_intent_and_final_verification() {
        let content = format!(
            "ORIGINAL GOAL: preserve authentication. {} FINAL RESULT: cargo test is green.",
            "middle implementation detail ".repeat(64)
        );

        let excerpt = context_excerpt(&content);

        assert!(excerpt.contains("ORIGINAL GOAL"));
        assert!(excerpt.contains("FINAL RESULT"));
        assert!(excerpt.contains('…'));
        assert!(excerpt.chars().count() <= MAX_CONTEXT_CAPSULE_EXCERPT_CHARS);
    }

    #[test]
    fn stateful_cursor_returns_only_unsent_entries() {
        let mut state = AgentState::new();
        let user = state.push_user(1, "request");
        state.mark_represented_through(user);
        state.push_tool_result(1, 9, "read_file", &ToolOutcome::success("contents"));

        let unsent = state.committed_after(state.represented_through);
        assert_eq!(unsent.len(), 1);
        assert!(matches!(unsent[0].kind, HistoryKind::ToolResult { .. }));
    }

    #[test]
    fn compaction_never_exceeds_even_tiny_budgets() {
        let mut state = AgentState::new();
        state.push_user(1, "first prompt that is intentionally long");
        state.push_assistant(1, "old assistant response".repeat(16));
        state.push_tool_result(1, 1, "read_file", &ToolOutcome::success("x".repeat(4_096)));
        state.push_assistant(1, "newest causal response".repeat(16));

        for budget in 0..=64 {
            let compacted = state.compacted_committed(budget);
            assert!(
                token_sum(&compacted) <= u64::from(budget),
                "budget={budget}"
            );
            if AgentState::prompt_fits_budget(&state.history[0].content, budget) {
                assert_eq!(compacted.first().map(|entry| entry.sequence), Some(1));
                assert_eq!(compacted[0].content, state.history[0].content);
            } else {
                assert!(compacted.is_empty());
            }
        }
    }

    #[test]
    fn retained_non_anchor_entries_are_a_newest_contiguous_suffix() {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_assistant(1, "a".repeat(512));
        state.push_user(2, "latest request");
        state.push_assistant(2, "newest");

        let compacted = state.compacted_committed(64);
        let sequences: Vec<_> = compacted.iter().map(|entry| entry.sequence).collect();
        assert_eq!(sequences.first(), Some(&1));
        let tail = &sequences[1..];
        for pair in tail.windows(2) {
            assert_eq!(pair[1], pair[0] + 1);
        }
        assert_eq!(tail.last(), Some(&4));
    }

    #[test]
    fn compaction_recomputes_untrusted_token_estimates() {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_assistant(1, "x".repeat(1_024));
        state.history[1].approx_tokens = 0;

        let compacted = state.compacted_committed(8);
        assert!(token_sum(&compacted) <= 8);
    }

    #[test]
    fn low_reported_usage_cannot_bypass_the_opaque_replay_budget() {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_assistant_with_api_items(
            1,
            "opaque response",
            vec![json!({
                "type": "reasoning",
                "encrypted_content": "x".repeat(16_384)
            })],
        );
        state.record_usage(1, state.last_committed_sequence());

        let compacted = state.compacted_committed(16);
        assert!(token_sum(&compacted) <= 16);
        assert!(
            compacted.iter().all(|entry| entry.api_items.is_empty()),
            "oversized opaque replay item escaped the strict local budget"
        );
    }

    #[test]
    fn message_envelopes_and_tool_metadata_count_toward_the_budget() -> Result<(), &'static str> {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        for turn_id in 2..=128 {
            state.push_user(turn_id, "x");
            state.push_tool_result(turn_id, turn_id, "read_file", &ToolOutcome::success(""));
        }
        state.record_usage(1, state.last_committed_sequence());

        let compacted = state.compacted_committed(64);
        assert!(token_sum(&compacted) <= 64);
        assert!(compacted.len() < state.history.len());

        let error = state
            .checked_request_context_after(0, 64)
            .err()
            .ok_or("uncompacted stateful input unexpectedly fit")?;
        assert!(error.required_tokens > error.context_budget);
        Ok(())
    }

    #[test]
    fn high_reported_usage_reduces_small_message_history() {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        for turn_id in 2..=5 {
            state.push_user(turn_id, "x");
        }
        assert!(token_sum(&state.history) < 200);
        state.record_usage(1_000, state.last_committed_sequence());

        let compacted = state.compacted_committed(200);
        assert!(compacted.len() < state.history.len());
        if let Some(first) = compacted.first() {
            assert_eq!(first.sequence, 1);
        } else {
            assert!(state.checked_compacted_request_context(200).is_err());
        }
    }

    #[test]
    fn persistent_history_stays_bounded_across_a_long_tool_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentState::new();
        for turn_id in 1..=1_000 {
            state.push_user(turn_id, format!("prompt-{turn_id}"));
            state.push_tool_result_with_action(
                turn_id,
                turn_id,
                &ToolAction::ReadFile {
                    path: format!("file-{turn_id}.txt"),
                },
                &ToolOutcome::success("x".repeat(2_048)),
            );
            state.push_assistant(turn_id, format!("answer-{turn_id}"));
            state.compact_persisted_history(4_096, false)?;
        }

        assert!(state.history.len() <= super::MAX_PERSISTED_API_ENTRIES + 1);
        assert!(
            super::persistent_entry_bytes(&state.history) <= super::MAX_PERSISTED_API_BYTES + 1_024
        );
        assert!(state.transcript_archive.len() <= super::MAX_TRANSCRIPT_ARCHIVE_ENTRIES);
        assert!(
            super::persistent_entry_bytes(&state.transcript_archive)
                <= super::MAX_TRANSCRIPT_ARCHIVE_BYTES
        );
        assert!(state.history.iter().any(|entry| {
            matches!(entry.kind, HistoryKind::User) && entry.content == "prompt-1"
        }));
        assert!(state.history.iter().any(|entry| entry.turn_id == 1_000));
        Ok(())
    }

    #[test]
    fn failed_pending_prompt_is_not_reused_by_the_next_request() {
        let mut state = AgentState::new();
        state.push_pending_user(1, "failed initial prompt");
        assert_eq!(state.request_context_after(0).len(), 1);
        assert!(state.committed_after(0).is_empty());

        assert_eq!(state.mark_turn_failed(1), 1);
        state.push_pending_user(2, "replacement prompt");
        let request = state.request_context_after(0);
        assert_eq!(request.len(), 1);
        assert_eq!(request[0].turn_id, 2);
        assert_eq!(request[0].content, "replacement prompt");
    }

    #[test]
    fn pending_prompt_can_retry_then_commit_on_the_same_logical_turn() {
        let mut state = AgentState::new();
        let sequence = state.push_pending_user(7, "retry me");
        assert_eq!(state.request_context_after(0)[0].sequence, sequence);
        assert_eq!(state.request_context_after(0)[0].turn_id, 7);
        assert_eq!(state.mark_turn_committed(7), 1);
        assert_eq!(state.committed_after(0)[0].sequence, sequence);
    }

    #[test]
    fn turn_metrics_accumulate_across_persisted_retry_segments()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentState::new();
        state.accumulate_turn_metrics(
            7,
            TurnMetrics {
                elapsed_millis: 12_000,
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 120,
                cost_microusd: Some(700),
            },
        );
        let encoded = serde_json::to_string(&state)?;
        let mut recovered: AgentState = serde_json::from_str(&encoded)?;
        recovered.push_assistant(7, "finished");

        assert!(recovered.complete_turn_metrics(
            7,
            TurnMetrics {
                elapsed_millis: 5_000,
                input_tokens: 40,
                output_tokens: 10,
                total_tokens: 50,
                cost_microusd: Some(300),
            },
        ));

        assert_eq!(
            recovered
                .history
                .last()
                .and_then(|entry| entry.turn_metrics.as_ref()),
            Some(&TurnMetrics {
                elapsed_millis: 17_000,
                input_tokens: 140,
                output_tokens: 30,
                total_tokens: 170,
                cost_microusd: Some(1_000),
            })
        );
        Ok(())
    }

    #[test]
    fn cancelled_turn_drops_accumulated_metrics() {
        let mut state = AgentState::new();
        state.accumulate_turn_metrics(
            9,
            TurnMetrics {
                elapsed_millis: 8_000,
                input_tokens: 10,
                output_tokens: 2,
                total_tokens: 12,
                cost_microusd: Some(50),
            },
        );
        state.clear_turn_metrics(9);
        state.push_assistant(9, "replacement");

        assert!(state.complete_turn_metrics(
            9,
            TurnMetrics {
                elapsed_millis: 1_000,
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cost_microusd: Some(10),
            },
        ));
        assert_eq!(
            state
                .history
                .last()
                .and_then(|entry| entry.turn_metrics.as_ref()),
            Some(&TurnMetrics {
                elapsed_millis: 1_000,
                input_tokens: 1,
                output_tokens: 1,
                total_tokens: 2,
                cost_microusd: Some(10),
            })
        );
    }

    #[test]
    fn restart_cancels_in_flight_prompt_and_starts_a_fresh_epoch() {
        let mut state = AgentState::new();
        state.push_pending_user(7, "possibly sent before the crash");
        let old_epoch = state.conversation_epoch;

        state.recover_after_restart();

        assert_eq!(state.conversation_epoch, old_epoch.saturating_add(1));
        assert!(matches!(state.history[0].status, HistoryStatus::Cancelled));
        assert_eq!(state.history[0].epoch, state.conversation_epoch);
        assert!(state.request_context_after(0).is_empty());
        assert!(state.last_response_id.is_none());
    }

    #[test]
    fn reset_clears_the_paused_turn_marker() {
        let mut state = AgentState::new();
        state.push_pending_user(7, "paused task");
        assert_eq!(state.mark_turn_paused(7), 1);

        state.reset();

        assert!(state.paused_turn_id.is_none());
        assert!(state.history.is_empty());
    }

    #[test]
    fn restart_clears_an_orphaned_pause_marker() {
        let mut state = AgentState::new();
        state.paused_turn_id = Some(99);

        state.recover_after_restart();

        assert!(state.paused_turn_id.is_none());
    }

    #[test]
    fn restart_preserves_monotonic_history_revisions() {
        let mut state = AgentState::new();
        state.push_pending_user(7, "possibly sent");
        state.history[0].revision = 100;
        state.history_revision = 0;

        state.recover_after_restart();

        assert!(state.history[0].revision > 100);
        assert!(state.history_revision > 100);
    }

    #[test]
    fn attachment_cost_survives_context_recalculation() {
        let mut state = AgentState::new();
        state.push_pending_user_with_attachments(
            1,
            "inspect",
            vec![AttachmentRef {
                sha256: "a".repeat(64),
                filename: "large.txt".to_owned(),
                mime_type: "text/plain".to_owned(),
                size_bytes: 4_096,
                kind: AttachmentKind::Text,
                detail: AttachmentDetail::Auto,
            }],
        );

        assert!(state.checked_request_context_after(0, 64).is_err());
    }

    #[test]
    fn persistent_cap_rejects_an_oversized_anchor() {
        let mut state = AgentState::new();
        state.push_user(1, "x".repeat(super::MAX_PERSISTED_API_BYTES + 1));

        assert!(state.compact_persisted_history(u32::MAX, false).is_err());
    }

    #[test]
    fn persistent_cap_can_split_a_long_single_turn() -> Result<(), ContextBudgetExceeded> {
        let mut state = AgentState::new();
        state.push_user(1, "inspect everything");
        for action_id in 1..=1_030 {
            state.push_assistant(1, format!("tool request {action_id}"));
            state.push_tool_result(1, action_id, "read_file", &ToolOutcome::success("ok"));
        }
        let latest = state.last_committed_sequence();

        state.compact_persisted_history(u32::MAX, false)?;

        assert!(state.history.len() <= super::MAX_PERSISTED_API_ENTRIES);
        assert!(state.history.iter().any(|entry| entry.sequence == latest));
        Ok(())
    }

    #[test]
    fn explicit_pause_survives_restart_and_requires_explicit_resume()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentState::new();
        state.push_pending_user(41, "finish the refactor");
        assert_eq!(state.mark_turn_paused(41), 1);
        assert_eq!(state.paused_turn_id, Some(41));
        assert!(matches!(state.history[0].status, HistoryStatus::Paused));

        let encoded = serde_json::to_string(&state)?;
        let mut recovered: AgentState = serde_json::from_str(&encoded)?;
        recovered.recover_after_restart();
        assert_eq!(recovered.paused_turn_id, Some(41));
        assert!(matches!(recovered.history[0].status, HistoryStatus::Paused));

        assert_eq!(recovered.resume_paused_turn(41), 1);
        assert_eq!(recovered.paused_turn_id, None);
        assert!(matches!(
            recovered.history[0].status,
            HistoryStatus::Pending
        ));
        Ok(())
    }

    #[test]
    fn abrupt_restart_recovers_a_committed_tool_turn_as_paused()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentState::new();
        state.push_user(52, "run the check");
        state.push_assistant(52, "starting the command");
        state.begin_turn(52);

        let encoded = serde_json::to_string(&state)?;
        let mut recovered: AgentState = serde_json::from_str(&encoded)?;
        recovered.recover_after_restart();

        assert_eq!(recovered.paused_turn_id, Some(52));
        assert_eq!(recovered.resume_paused_turn(52), 1);
        assert_eq!(recovered.paused_turn_id, None);
        Ok(())
    }

    #[test]
    fn stateless_replay_preserves_opaque_assistant_items() {
        let mut state = AgentState::new();
        state.push_user(1, "question");
        let opaque = json!({
            "type": "future_item",
            "role": "future_role",
            "encrypted_content": "opaque"
        });
        state.push_assistant_with_api_items(1, "answer", vec![opaque.clone()]);
        state.push_pending_user(2, "follow-up");

        let replay = state.stateless_replay_input(1_024);
        assert_eq!(replay[0]["role"], "user");
        assert_eq!(replay[1], opaque);
        assert_eq!(replay[2]["content"], "follow-up");
    }

    #[test]
    fn stateless_replay_preserves_native_function_output_call_id() {
        let mut state = AgentState::new();
        state.push_user(1, "inspect");
        let sequence =
            state.push_tool_diagnostic(1, 7, "mcp:files::read", ToolResultStatus::Success, "ok");
        let output = serde_json::json!({
            "type": "function_call_output",
            "call_id": "call_exact_42",
            "output": "{\"ok\":true}",
        });
        assert!(state.set_api_items(sequence, vec![output.clone()]));

        let replay = state.stateless_replay_input(1_024);
        assert!(replay.iter().any(|item| item == &output));
        assert!(!replay.iter().any(|item| {
            item.get("content")
                .and_then(Value::as_str)
                .is_some_and(|content| content.contains("mcp:files::read"))
        }));
    }

    #[test]
    fn stateless_replay_repairs_legacy_orphan_call_from_ordered_tool_result() {
        let mut state = AgentState::new();
        state.push_user(1, "inspect");
        state.push_assistant_with_api_items(
            1,
            "",
            vec![
                json!({
                    "type": "reasoning",
                    "encrypted_content": "opaque",
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_legacy_1",
                    "name": "codebase_overview",
                    "arguments": "{}",
                }),
            ],
        );
        state.push_tool_diagnostic(
            1,
            1,
            "mcp:builtin:code-index::codebase_overview",
            ToolResultStatus::Success,
            "[tool result compacted: sha256=abc]",
        );
        state.push_assistant(1, "The project overview is available.");

        let replay = state
            .checked_stateless_replay_input(4_096)
            .unwrap_or_default();
        let call_index = replay
            .iter()
            .position(|item| item["call_id"] == "call_legacy_1" && item["type"] == "function_call")
            .unwrap_or(usize::MAX);
        let output_index = replay
            .iter()
            .position(|item| {
                item["call_id"] == "call_legacy_1" && item["type"] == "function_call_output"
            })
            .unwrap_or(usize::MAX);

        assert!(call_index < output_index);
        assert_eq!(
            replay[output_index]["output"],
            "[tool result compacted: sha256=abc]"
        );
        assert_eq!(
            replay
                .iter()
                .filter(|item| item["type"] == "function_call_output")
                .count(),
            1
        );
    }

    #[test]
    fn legacy_repair_output_counts_toward_the_context_budget() {
        let mut state = AgentState::new();
        state.push_user(1, "inspect");
        state.push_assistant_with_api_items(
            1,
            "",
            vec![json!({
                "type": "function_call",
                "call_id": "call_legacy_large",
                "name": "read_file",
                "arguments": "{}",
            })],
        );
        let output = state.push_tool_diagnostic(
            1,
            1,
            "read_file",
            ToolResultStatus::Success,
            "x".repeat(8_192),
        );
        assert!(state.set_api_items(
            output,
            vec![json!({"type": "legacy_metadata", "value": true})],
        ));

        assert!(state.checked_stateless_replay_input(128).is_err());
    }

    #[test]
    fn stateless_replay_closes_unrecoverable_legacy_call_before_later_message() {
        let mut state = AgentState::new();
        state.push_user(1, "inspect");
        state.push_assistant_with_api_items(
            1,
            "",
            vec![json!({
                "type": "function_call",
                "call_id": "call_missing",
                "name": "read_file",
                "arguments": "{}",
            })],
        );
        state.push_user(2, "continue safely");

        let replay = state
            .checked_stateless_replay_input(4_096)
            .unwrap_or_default();
        let output_index = replay
            .iter()
            .position(|item| item["type"] == "function_call_output")
            .unwrap_or(usize::MAX);
        let next_user = replay
            .iter()
            .position(|item| item["content"] == "continue safely")
            .unwrap_or(0);

        assert!(output_index < next_user);
        assert_eq!(replay[output_index]["call_id"], "call_missing");
        assert!(
            replay[output_index]["output"]
                .as_str()
                .is_some_and(|output| output.contains("not persisted"))
        );
    }

    #[test]
    fn strict_compaction_keeps_latest_tool_round_and_rewrites_native_output_losslessly()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentState::new();
        state.push_user(1, "inspect the project and answer the question");
        state.push_assistant_with_api_items(
            1,
            "first tool request",
            vec![json!({
                "type": "function_call",
                "call_id": "call_old",
                "name": "code_search",
                "arguments": "x".repeat(32_768),
            })],
        );
        let old_output = state.push_tool_diagnostic(
            1,
            1,
            "code_search",
            ToolResultStatus::Success,
            "old result".repeat(4_096),
        );
        assert!(state.set_api_items(
            old_output,
            vec![json!({
                "type": "function_call_output",
                "call_id": "call_old",
                "output": "old result".repeat(4_096),
            })],
        ));
        state.push_assistant_with_api_items(
            1,
            "latest tool request",
            vec![json!({
                "type": "function_call",
                "call_id": "call_latest",
                "name": "code_search",
                "arguments": "{}",
            })],
        );
        let latest_output = state.push_tool_diagnostic(
            1,
            2,
            "code_search",
            ToolResultStatus::Success,
            "latest result".repeat(4_096),
        );
        assert!(state.set_api_items(
            latest_output,
            vec![json!({
                "type": "function_call_output",
                "call_id": "call_latest",
                "output": "latest result".repeat(4_096),
            })],
        ));

        let compacted = state.checked_compacted_request_context(512)?;
        assert!(compacted.iter().any(|entry| entry.sequence == 1));
        assert!(
            compacted
                .iter()
                .any(|entry| entry.sequence == latest_output)
        );
        assert!(!compacted.iter().any(|entry| {
            entry
                .api_items
                .iter()
                .any(|item| item["call_id"] == "call_old")
        }));
        let latest = compacted
            .iter()
            .find(|entry| entry.sequence == latest_output)
            .ok_or("latest output missing")?;
        let output = latest.api_items[0]["output"]
            .as_str()
            .ok_or("compacted native output is not text")?;
        assert!(output.contains("tool result compacted"));
        assert_eq!(latest.api_items[0]["call_id"], "call_latest");
        assert!(token_sum(&compacted) <= 512);
        Ok(())
    }

    #[test]
    fn oversized_first_prompt_is_rejected_not_truncated() -> Result<(), &'static str> {
        let prompt = "semantic prompt".repeat(32);
        let error = AgentState::validate_prompt_budget(&prompt, 8)
            .err()
            .ok_or("oversized prompt unexpectedly fit")?;
        assert!(error.required_tokens > error.context_budget);

        let mut state = AgentState::new();
        state.push_pending_user(1, prompt.clone());
        assert!(state.compacted_request_context(8).is_empty());
        assert_eq!(state.history[0].content, prompt);
        Ok(())
    }

    #[test]
    fn next_prompt_is_rejected_when_anchor_and_prompt_cannot_both_fit() -> Result<(), &'static str>
    {
        let mut state = AgentState::new();
        state.push_user(1, "first semantic anchor");
        let prompt = "new prompt".repeat(16);
        let prompt_alone = super::estimate_user_message_tokens(&prompt);
        let error = state
            .validate_next_prompt_budget(&prompt, prompt_alone)
            .err()
            .ok_or("anchor plus prompt unexpectedly fit")?;
        assert!(error.required_tokens > error.context_budget);
        Ok(())
    }

    #[test]
    fn checked_compaction_never_sends_anchor_without_newest_group() -> Result<(), &'static str> {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_assistant_with_api_items(
            1,
            "large newest response",
            vec![json!({"type": "reasoning", "encrypted_content": "x".repeat(4096)})],
        );
        state.push_tool_result(1, 1, "read_file", &ToolOutcome::success("result"));

        let error = state
            .checked_compacted_request_context(4)
            .err()
            .ok_or("newest causal group was silently retained under impossible budget")?;
        assert_eq!(error.context_budget, 4);
        Ok(())
    }

    #[test]
    fn compaction_never_orphans_a_final_answer_from_its_tool_round() {
        let mut state = AgentState::new();
        state.push_user(1, "anchor");
        state.push_user(2, "latest request");
        state.push_assistant(
            2,
            "<read_file><path>large.txt</path></read_file>".repeat(12),
        );
        state.push_tool_result_with_action(
            2,
            9,
            &ToolAction::ReadFile {
                path: "large.txt".to_owned(),
            },
            &ToolOutcome::success("x".repeat(600)),
        );
        state.push_assistant(2, "short final answer");

        let compacted = state.compacted_committed(12);
        assert!(compacted.iter().all(|entry| entry.turn_id != 2));
        assert!(state.checked_compacted_request_context(12).is_err());
    }

    #[test]
    fn history_entries_carry_epoch_and_monotonic_revision() {
        let mut state = AgentState::new();
        state.push_user(1, "first");
        state.push_pending_user(2, "second");
        assert_eq!(state.history[0].epoch, 1);
        assert!(state.history[1].revision > state.history[0].revision);
        let before_transition = state.history[1].revision;
        state.mark_turn_committed(2);
        assert!(state.history[1].revision > before_transition);

        state.reset();
        state.push_user(3, "third");
        assert_eq!(state.history[0].epoch, 2);
        assert_eq!(state.history[0].revision, 1);
    }
}
