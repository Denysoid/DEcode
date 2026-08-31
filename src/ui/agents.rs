use std::{
    collections::BTreeSet,
    sync::Arc,
    time::{Duration, Instant},
};

use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

use crate::agent::{
    AgentProfileSource, AgentProfileSummary, SubagentFileReview, SubagentFleetSnapshot, SubagentId,
    SubagentMode, SubagentSnapshot, SubagentStatus, side_chat::has_visible_text,
};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_FRAME: Duration = Duration::from_millis(140);
const DETAIL_LIMIT: usize = 8_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEditor {
    Closed,
    Spawn,
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentBrowseFocus {
    List,
    New,
    Reload,
    Message,
    Stop,
    Review,
    RaiseBudget,
    StopAtBudget,
    Resume,
    Abandon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentDialogFocus {
    Profiles,
    Dependencies,
    Claims,
    Task,
    Cancel,
    Submit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentDecisionFocus {
    Decline,
    Approve,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentHit {
    Item(usize),
    Profile(usize),
    Dependency(usize),
    Claim(usize),
    Browse(AgentBrowseFocus),
    Dialog(AgentDialogFocus),
    Command(AgentDecisionFocus),
    Binary(AgentDecisionFocus),
}

#[derive(Debug, Clone)]
pub struct AgentUiState {
    picker: ListPickerState,
    profile_picker: ListPickerState,
    dependency_picker: ListPickerState,
    claim_picker: ListPickerState,
    selected_id: Option<SubagentId>,
    selected_profile_id: Option<String>,
    selected_dependencies: BTreeSet<SubagentId>,
    selected_file_claims: BTreeSet<String>,
    browse_focus: FocusManager<AgentBrowseFocus>,
    dialog_focus: FocusManager<AgentDialogFocus>,
    command_focus: FocusManager<AgentDecisionFocus>,
    binary_focus: FocusManager<AgentDecisionFocus>,
    clicks: ClickRegionRegistry<AgentHit>,
    editor_dialog: DialogState<()>,
    command_dialog: DialogState<()>,
    binary_dialog: DialogState<()>,
    editor: AgentEditor,
    buffer: String,
    animation_started: Instant,
}

impl AgentUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut browse_focus = FocusManager::new();
        for focus in [
            AgentBrowseFocus::List,
            AgentBrowseFocus::New,
            AgentBrowseFocus::Reload,
            AgentBrowseFocus::Message,
            AgentBrowseFocus::Stop,
            AgentBrowseFocus::Review,
            AgentBrowseFocus::Resume,
            AgentBrowseFocus::Abandon,
            AgentBrowseFocus::RaiseBudget,
            AgentBrowseFocus::StopAtBudget,
        ] {
            browse_focus.register(focus);
        }
        browse_focus.set(AgentBrowseFocus::List);

        let mut dialog_focus = FocusManager::new();
        dialog_focus.register(AgentDialogFocus::Profiles);
        dialog_focus.register(AgentDialogFocus::Dependencies);
        dialog_focus.register(AgentDialogFocus::Claims);
        dialog_focus.register(AgentDialogFocus::Task);
        dialog_focus.register(AgentDialogFocus::Cancel);
        dialog_focus.register(AgentDialogFocus::Submit);
        dialog_focus.set(AgentDialogFocus::Submit);

        let mut command_focus = FocusManager::new();
        command_focus.register(AgentDecisionFocus::Decline);
        command_focus.register(AgentDecisionFocus::Approve);
        command_focus.set(AgentDecisionFocus::Decline);

        let mut binary_focus = FocusManager::new();
        binary_focus.register(AgentDecisionFocus::Decline);
        binary_focus.register(AgentDecisionFocus::Approve);
        binary_focus.set(AgentDecisionFocus::Decline);

        Self {
            picker: ListPickerState::new(0),
            profile_picker: ListPickerState::new(0),
            dependency_picker: ListPickerState::new(0),
            claim_picker: ListPickerState::new(0),
            selected_id: None,
            selected_profile_id: None,
            selected_dependencies: BTreeSet::new(),
            selected_file_claims: BTreeSet::new(),
            browse_focus,
            dialog_focus,
            command_focus,
            binary_focus,
            clicks: ClickRegionRegistry::new(),
            editor_dialog: DialogState::new(()),
            command_dialog: DialogState::new(()),
            binary_dialog: DialogState::new(()),
            editor: AgentEditor::Closed,
            buffer: String::new(),
            animation_started: Instant::now(),
        }
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn sync(&mut self, fleet: &SubagentFleetSnapshot) {
        self.selected_dependencies
            .retain(|id| fleet.agents.iter().any(|agent| agent.id == *id));
        self.picker.set_total(fleet.agents.len());
        let selected = self
            .selected_id
            .and_then(|id| fleet.agents.iter().position(|agent| agent.id == id));
        match selected {
            Some(index) => self.picker.select(index),
            None if fleet.agents.is_empty() => {
                self.selected_id = None;
                self.picker.select_first();
            }
            None => {
                self.picker.select_first();
                self.selected_id = fleet.agents.first().map(|agent| agent.id);
            }
        }
        self.profile_picker.set_total(fleet.profiles.profiles.len());
        let selected_profile = self.selected_profile_id.as_ref().and_then(|id| {
            fleet
                .profiles
                .profiles
                .iter()
                .position(|profile| &profile.id == id)
        });
        match selected_profile {
            Some(index) => self.profile_picker.select(index),
            None if fleet.profiles.profiles.is_empty() => {
                self.selected_profile_id = None;
                self.profile_picker.select_first();
            }
            None => {
                self.profile_picker.select_first();
                self.selected_profile_id = fleet
                    .profiles
                    .profiles
                    .first()
                    .map(|profile| profile.id.clone());
            }
        }
    }

    #[must_use]
    pub fn selected<'a>(&self, fleet: &'a SubagentFleetSnapshot) -> Option<&'a SubagentSnapshot> {
        self.selected_id
            .and_then(|id| fleet.agents.iter().find(|agent| agent.id == id))
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<SubagentId> {
        self.selected_id
    }

    pub fn select_index(&mut self, fleet: &SubagentFleetSnapshot, index: usize) {
        if let Some(agent) = fleet.agents.get(index) {
            self.picker.select(index);
            self.selected_id = Some(agent.id);
            self.browse_focus.set(AgentBrowseFocus::List);
        }
    }

    pub fn next_item(&mut self, fleet: &SubagentFleetSnapshot) {
        self.picker.select_next();
        self.selected_id = fleet
            .agents
            .get(self.picker.selected_index)
            .map(|agent| agent.id);
    }

    pub fn previous_item(&mut self, fleet: &SubagentFleetSnapshot) {
        self.picker.select_prev();
        self.selected_id = fleet
            .agents
            .get(self.picker.selected_index)
            .map(|agent| agent.id);
    }

    pub fn first_item(&mut self, fleet: &SubagentFleetSnapshot) {
        self.picker.select_first();
        self.selected_id = fleet.agents.first().map(|agent| agent.id);
    }

    pub fn last_item(&mut self, fleet: &SubagentFleetSnapshot) {
        self.picker.select_last();
        self.selected_id = fleet.agents.last().map(|agent| agent.id);
    }

    #[must_use]
    pub const fn editor(&self) -> AgentEditor {
        self.editor
    }

    #[must_use]
    pub fn buffer(&self) -> &str {
        &self.buffer
    }

    #[must_use]
    pub fn buffer_has_visible_text(&self) -> bool {
        has_visible_text(&self.buffer)
    }

    pub fn open_spawn(&mut self, fleet: &SubagentFleetSnapshot) {
        self.sync(fleet);
        self.editor = AgentEditor::Spawn;
        self.buffer.clear();
        self.selected_dependencies.clear();
        self.selected_file_claims.clear();
        self.dependency_picker.set_total(fleet.agents.len());
        self.dependency_picker.select_first();
        self.claim_picker.set_total(0);
        self.claim_picker.select_first();
        self.dialog_focus.set(AgentDialogFocus::Profiles);
        self.editor_dialog.show();
    }

    pub fn open_message(&mut self) {
        self.editor = AgentEditor::Message;
        self.buffer.clear();
        self.dialog_focus.set(AgentDialogFocus::Submit);
        self.editor_dialog.show();
    }

    pub fn close_editor(&mut self) {
        self.editor = AgentEditor::Closed;
        self.buffer.clear();
        self.editor_dialog.hide();
    }

    pub fn push(&mut self, character: char) {
        if self.editor == AgentEditor::Spawn {
            self.dialog_focus.set(AgentDialogFocus::Task);
        }
        self.buffer.push(character);
    }

    pub fn pop(&mut self) {
        if self.editor == AgentEditor::Spawn {
            self.dialog_focus.set(AgentDialogFocus::Task);
        }
        if let Some((start, _)) = self.buffer.grapheme_indices(true).next_back() {
            self.buffer.truncate(start);
        }
    }

    #[must_use]
    pub fn selected_profile_id(&self) -> Option<&str> {
        self.selected_profile_id.as_deref()
    }

    pub fn select_profile(&mut self, fleet: &SubagentFleetSnapshot, index: usize) {
        if let Some(profile) = fleet.profiles.profiles.get(index) {
            self.profile_picker.select(index);
            self.selected_profile_id = Some(profile.id.clone());
            self.dialog_focus.set(AgentDialogFocus::Profiles);
            if profile.mode != crate::agent::SubagentMode::Writer {
                self.selected_file_claims.clear();
            }
        }
    }

    pub fn next_profile(&mut self, fleet: &SubagentFleetSnapshot) {
        self.profile_picker.select_next();
        self.selected_profile_id = fleet
            .profiles
            .profiles
            .get(self.profile_picker.selected_index)
            .map(|profile| profile.id.clone());
        self.dialog_focus.set(AgentDialogFocus::Profiles);
        self.clear_claims_for_read_only_profile(fleet);
    }

    pub fn previous_profile(&mut self, fleet: &SubagentFleetSnapshot) {
        self.profile_picker.select_prev();
        self.selected_profile_id = fleet
            .profiles
            .profiles
            .get(self.profile_picker.selected_index)
            .map(|profile| profile.id.clone());
        self.dialog_focus.set(AgentDialogFocus::Profiles);
        self.clear_claims_for_read_only_profile(fleet);
    }

    pub fn next_dependency(&mut self, fleet: &SubagentFleetSnapshot) {
        self.dependency_picker.set_total(fleet.agents.len());
        self.dependency_picker.select_next();
        self.dialog_focus.set(AgentDialogFocus::Dependencies);
    }

    pub fn previous_dependency(&mut self, fleet: &SubagentFleetSnapshot) {
        self.dependency_picker.set_total(fleet.agents.len());
        self.dependency_picker.select_prev();
        self.dialog_focus.set(AgentDialogFocus::Dependencies);
    }

    pub fn toggle_selected_dependency(&mut self, fleet: &SubagentFleetSnapshot) {
        self.toggle_dependency(fleet, self.dependency_picker.selected_index);
    }

    pub fn toggle_dependency(&mut self, fleet: &SubagentFleetSnapshot, index: usize) {
        let Some(agent) = fleet.agents.get(index) else {
            return;
        };
        if !self.selected_dependencies.remove(&agent.id) {
            self.selected_dependencies.insert(agent.id);
        }
        self.dependency_picker.select(index);
        self.dialog_focus.set(AgentDialogFocus::Dependencies);
    }

    pub fn next_claim(&mut self, files: &[String]) {
        self.claim_picker.set_total(files.len());
        self.claim_picker.select_next();
        self.dialog_focus.set(AgentDialogFocus::Claims);
    }

    pub fn previous_claim(&mut self, files: &[String]) {
        self.claim_picker.set_total(files.len());
        self.claim_picker.select_prev();
        self.dialog_focus.set(AgentDialogFocus::Claims);
    }

    pub fn toggle_selected_claim(&mut self, fleet: &SubagentFleetSnapshot, files: &[String]) {
        self.toggle_claim(fleet, files, self.claim_picker.selected_index);
    }

    pub fn toggle_claim(&mut self, fleet: &SubagentFleetSnapshot, files: &[String], index: usize) {
        if !self.selected_profile_is_writer(fleet) {
            self.selected_file_claims.clear();
            return;
        }
        let Some(path) = files.get(index) else {
            return;
        };
        if !self.selected_file_claims.remove(path) {
            self.selected_file_claims.insert(path.clone());
        }
        self.claim_picker.select(index);
        self.dialog_focus.set(AgentDialogFocus::Claims);
    }

    #[must_use]
    pub fn selected_dependencies(&self) -> Vec<SubagentId> {
        self.selected_dependencies.iter().copied().collect()
    }

    #[must_use]
    pub fn selected_file_claims(&self) -> Vec<String> {
        self.selected_file_claims.iter().cloned().collect()
    }

    fn selected_profile_is_writer(&self, fleet: &SubagentFleetSnapshot) -> bool {
        self.selected_profile_id.as_ref().is_some_and(|selected| {
            fleet.profiles.profiles.iter().any(|profile| {
                &profile.id == selected && profile.mode == crate::agent::SubagentMode::Writer
            })
        })
    }

    fn clear_claims_for_read_only_profile(&mut self, fleet: &SubagentFleetSnapshot) {
        if !self.selected_profile_is_writer(fleet) {
            self.selected_file_claims.clear();
        }
    }

    pub fn next_focus(&mut self) {
        match self.editor {
            AgentEditor::Closed => self.browse_focus.next(),
            AgentEditor::Spawn => self.dialog_focus.next(),
            AgentEditor::Message => {
                let next = match self.dialog_focus.current() {
                    Some(AgentDialogFocus::Task) => AgentDialogFocus::Cancel,
                    Some(AgentDialogFocus::Cancel) => AgentDialogFocus::Submit,
                    _ => AgentDialogFocus::Task,
                };
                self.dialog_focus.set(next);
            }
        }
    }

    pub fn previous_focus(&mut self) {
        match self.editor {
            AgentEditor::Closed => self.browse_focus.prev(),
            AgentEditor::Spawn => self.dialog_focus.prev(),
            AgentEditor::Message => {
                let previous = match self.dialog_focus.current() {
                    Some(AgentDialogFocus::Task) => AgentDialogFocus::Submit,
                    Some(AgentDialogFocus::Cancel) => AgentDialogFocus::Task,
                    _ => AgentDialogFocus::Cancel,
                };
                self.dialog_focus.set(previous);
            }
        }
    }

    pub fn next_command_focus(&mut self) {
        self.command_focus.next();
    }

    pub fn previous_command_focus(&mut self) {
        self.command_focus.prev();
    }

    pub fn next_binary_focus(&mut self) {
        self.binary_focus.next();
    }

    pub fn previous_binary_focus(&mut self) {
        self.binary_focus.prev();
    }

    #[must_use]
    pub fn browse_focused(&self) -> Option<AgentBrowseFocus> {
        self.browse_focus.current().copied()
    }

    #[must_use]
    pub fn dialog_focused(&self) -> Option<AgentDialogFocus> {
        self.dialog_focus.current().copied()
    }

    #[must_use]
    pub fn command_focused(&self) -> Option<AgentDecisionFocus> {
        self.command_focus.current().copied()
    }

    #[must_use]
    pub fn binary_focused(&self) -> Option<AgentDecisionFocus> {
        self.binary_focus.current().copied()
    }

    pub fn focus_browse(&mut self, focus: AgentBrowseFocus) {
        self.browse_focus.set(focus);
    }

    pub fn focus_dialog(&mut self, focus: AgentDialogFocus) {
        self.dialog_focus.set(focus);
    }

    pub fn focus_command(&mut self, focus: AgentDecisionFocus) {
        self.command_focus.set(focus);
    }

    pub fn focus_binary(&mut self, focus: AgentDecisionFocus) {
        self.binary_focus.set(focus);
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<AgentHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw_tab(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        fleet: &SubagentFleetSnapshot,
        main_idle: bool,
    ) {
        self.sync(fleet);
        let horizontal = area.width >= 78;
        let panes = if horizontal {
            Layout::horizontal([Constraint::Percentage(39), Constraint::Percentage(61)]).split(area)
        } else {
            Layout::vertical([Constraint::Percentage(44), Constraint::Percentage(56)]).split(area)
        };
        self.draw_list(frame, panes[0], fleet);
        self.draw_detail(frame, panes[1], fleet, main_idle);
    }

    fn draw_list(&mut self, frame: &mut Frame<'_>, area: Rect, fleet: &SubagentFleetSnapshot) {
        let border = if self.browse_focus.current() == Some(&AgentBrowseFocus::List) {
            Color::LightCyan
        } else {
            Color::DarkGray
        };
        let title = if let Some(error) = &fleet.availability_error {
            format!(
                " {} • {} ",
                text(Text::AgentsUnavailable),
                truncate_for_display(&sanitize_for_display(error), 44)
            )
        } else {
            format!(
                " {} {} {} · {} {} · {} / {} {} ",
                text(Text::Agents),
                fleet.active,
                text(Text::OpenLower),
                fleet.capacity,
                text(Text::ParallelLower),
                fleet.total_tokens,
                fleet.token_budget,
                text(Text::Tokens)
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(border));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.picker.ensure_visible(usize::from(inner.height));
        let list_style = ListPickerStyle::bracket().bordered(false);
        let label_width =
            usize::from(inner.width).saturating_sub(UnicodeWidthStr::width(list_style.indicator));
        let items = fleet
            .agents
            .iter()
            .map(|agent| {
                let tree = if agent.depth > 1 {
                    format!(
                        "{}└─ ",
                        "  ".repeat(usize::from(agent.depth.saturating_sub(2)))
                    )
                } else {
                    String::new()
                };
                let animation = if agent.status.is_active() {
                    format!("{} ", self.animation_glyph())
                } else if agent.status.is_recoverable() {
                    "↻ ".to_owned()
                } else {
                    "  ".to_owned()
                };
                let label = format!(
                    "{tree}{animation}{}  {}  {}",
                    agent.id,
                    sanitize_for_display(&agent.label),
                    localized_agent_status(agent.status)
                );
                truncate_to_width(
                    &label.split_whitespace().collect::<Vec<_>>().join(" "),
                    label_width,
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            ListPicker::new(&items, &self.picker).style(list_style),
            inner,
        );
        for row in 0..usize::from(inner.height) {
            let index = usize::from(self.picker.scroll).saturating_add(row);
            if index >= fleet.agents.len() {
                break;
            }
            self.clicks.register(
                Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
                AgentHit::Item(index),
            );
        }
    }

    fn draw_detail(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        fleet: &SubagentFleetSnapshot,
        main_idle: bool,
    ) {
        let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(9)]).split(area);
        let Some(agent) = self.selected(fleet) else {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        text(Text::NoDelegatedTasks),
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(text(Text::DelegationStartHelp)),
                    Line::from(text(Text::AgentIdsDurableHelp)),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", text(Text::DelegationTitle))),
                )
                .wrap(Wrap { trim: false }),
                rows[0],
            );
            self.draw_actions(frame, rows[1], None, main_idle);
            return;
        };
        let result = if let Some(error) = &agent.error {
            format!("{}: {}", text(Text::Failed), sanitize_for_display(error))
        } else if !agent.result.is_empty() {
            sanitize_for_display(&agent.result)
        } else {
            localized_agent_message(&agent.last_message)
        };
        let safe_result = truncate_for_display(&result, DETAIL_LIMIT);
        let title = format!(" {} • {} ", agent.id, sanitize_for_display(&agent.label));
        let mut detail_lines = vec![
            Line::from(vec![
                Span::styled(
                    localized_agent_status(agent.status),
                    Style::default()
                        .fg(status_color(agent))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  •  {}", localized_agent_mode(agent.mode))),
            ]),
            Line::from(format!(
                "{}: {} ({})",
                text(Text::ProfileLabel),
                sanitize_for_display(&agent.profile_name),
                sanitize_for_display(&agent.profile_id)
            )),
            Line::from(format!(
                "{}: {} / {}",
                text(Text::ActualRuntimeLabel),
                sanitize_for_display(&agent.deployment),
                agent.reasoning_effort
            )),
            Line::from(format!(
                "{} {}  •  {} {}",
                text(Text::HierarchyDepthParent),
                agent.depth,
                text(Text::ParentLabel),
                agent
                    .parent_id
                    .map_or_else(|| text(Text::RootLower).to_owned(), |id| id.to_string())
            )),
            Line::from(format!(
                "{}: {} {} + {} {} = {} / {}  •  {} {}",
                text(Text::Tokens),
                agent.input_tokens,
                text(Text::Input),
                agent.output_tokens,
                text(Text::OutputLabel),
                agent.total_tokens,
                agent.token_budget,
                text(Text::TokensInOutToolRounds),
                agent.tool_iterations
            )),
            Line::from(format!(
                "{}: {}  •  {}: {}",
                text(Text::PendingFilesReviewed),
                agent.changed_files.len(),
                text(Text::ReviewedLabel),
                agent.resolved_files.len()
            )),
            Line::from(format!(
                "{}: {}",
                text(Text::DependsOnLabel),
                if agent.dependencies.is_empty() {
                    text(Text::NoneLower).to_owned()
                } else {
                    agent
                        .dependencies
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )),
            Line::from(format!(
                "{}: {}",
                text(Text::FileClaimsLabel),
                if agent.mode == crate::agent::SubagentMode::Writer && agent.file_claims.is_empty()
                {
                    text(Text::ExclusiveWorkspaceAccess).to_owned()
                } else if agent.file_claims.is_empty() {
                    text(Text::NotApplicable).to_owned()
                } else {
                    agent
                        .file_claims
                        .iter()
                        .map(|path| sanitize_for_display(path))
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            )),
            Line::from(""),
            Line::from(Span::styled(
                safe_result,
                Style::default().fg(if agent.error.is_some() {
                    Color::LightRed
                } else {
                    Color::Gray
                }),
            )),
        ];
        if let Some(recovery) = &agent.recovery {
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(Span::styled(
                format!(
                    "{} {} · {} {}",
                    text(Text::RecoveryAttemptCheckpoint),
                    recovery.attempt,
                    text(Text::CheckpointLabel),
                    recovery.checkpoint_at.format("%Y-%m-%d %H:%M:%S UTC")
                ),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            )));
            detail_lines.push(Line::from(format!(
                "{}: {}",
                text(Text::ReasonLabel),
                truncate_for_display(&localized_agent_message(&recovery.reason), 280)
            )));
            if let Some(uncertain) = &recovery.uncertain_action {
                detail_lines.push(Line::from(Span::styled(
                    format!(
                        "{} #{} ({})",
                        text(Text::VerifyUnknownAction),
                        uncertain.action_id,
                        uncertain.action.tool_name()
                    ),
                    Style::default().fg(Color::LightRed),
                )));
            }
            if !recovery.can_resume {
                detail_lines.push(Line::from(Span::styled(
                    text(Text::ResumeWorktreeUnavailable),
                    Style::default().fg(Color::LightRed),
                )));
            }
        }
        if let Some(pending) = &agent.pending_budget {
            detail_lines.push(Line::from(""));
            detail_lines.push(Line::from(Span::styled(
                format!(
                    "{}: {} {}/{} {}",
                    text(Text::TokenGuardReached),
                    match pending.scope {
                        crate::agent::SubagentBudgetScope::Agent => text(Text::Agent),
                        crate::agent::SubagentBudgetScope::SessionTree => text(Text::Sessions),
                    },
                    pending.used,
                    pending.limit,
                    text(Text::Tokens)
                ),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            )));
            detail_lines.push(Line::from(format!(
                "{}: {}",
                text(Text::RaiseBudgetHelp),
                pending.suggested_increase,
            )));
        }
        frame.render_widget(
            Paragraph::new(detail_lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false }),
            rows[0],
        );
        self.draw_actions(frame, rows[1], Some(agent), main_idle);
    }

    fn draw_actions(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        agent: Option<&SubagentSnapshot>,
        main_idle: bool,
    ) {
        let rows = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);
        let first = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[0]);
        let second = Layout::horizontal([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(rows[1]);
        let recovery = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[2]);
        self.draw_browse_button(
            frame,
            first[0],
            text(Text::NewAgent),
            AgentBrowseFocus::New,
            true,
            ButtonStyle::primary(),
        );
        self.draw_browse_button(
            frame,
            first[1],
            text(Text::ReloadProfiles),
            AgentBrowseFocus::Reload,
            true,
            ButtonStyle::default(),
        );
        let active = agent.is_some_and(|agent| agent.status.is_active());
        self.draw_browse_button(
            frame,
            second[0],
            text(Text::MessageLabel),
            AgentBrowseFocus::Message,
            active,
            ButtonStyle::default(),
        );
        self.draw_browse_button(
            frame,
            second[1],
            text(Text::StopLabel),
            AgentBrowseFocus::Stop,
            active,
            ButtonStyle::danger(),
        );
        let review = main_idle && agent.is_some_and(|agent| !agent.changed_files.is_empty());
        self.draw_browse_button(
            frame,
            second[2],
            text(Text::ReviewChanges),
            AgentBrowseFocus::Review,
            review,
            ButtonStyle::success(),
        );
        let waiting_budget = agent.is_some_and(|agent| agent.pending_budget.is_some());
        let recoverable = agent.is_some_and(|agent| agent.status.is_recoverable());
        let can_resume = agent
            .and_then(|agent| agent.recovery.as_ref())
            .is_some_and(|recovery| recovery.can_resume);
        if waiting_budget {
            self.draw_browse_button(
                frame,
                recovery[0],
                text(Text::RaiseBudget50K),
                AgentBrowseFocus::RaiseBudget,
                true,
                ButtonStyle::success(),
            );
            self.draw_browse_button(
                frame,
                recovery[1],
                text(Text::StopBranch),
                AgentBrowseFocus::StopAtBudget,
                true,
                ButtonStyle::danger(),
            );
        } else {
            self.draw_browse_button(
                frame,
                recovery[0],
                text(Text::ResumeSafely),
                AgentBrowseFocus::Resume,
                recoverable && can_resume,
                ButtonStyle::success(),
            );
            self.draw_browse_button(
                frame,
                recovery[1],
                text(Text::AbandonRecovery),
                AgentBrowseFocus::Abandon,
                recoverable,
                ButtonStyle::danger(),
            );
        }
    }

    fn draw_browse_button(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        focus: AgentBrowseFocus,
        enabled: bool,
        style: ButtonStyle,
    ) {
        let mut state = if enabled {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        state.set_focused(self.browse_focus.current() == Some(&focus));
        let button = Button::new(label, &state)
            .variant(ButtonVariant::Block)
            .style(style);
        let region = button.render_stateful(area, frame.buffer_mut());
        if enabled {
            self.clicks.register(region.area, AgentHit::Browse(focus));
        }
    }

    pub fn draw_editor(
        &mut self,
        frame: &mut Frame<'_>,
        fleet: &SubagentFleetSnapshot,
        selected: Option<&SubagentSnapshot>,
        workspace_files: &[String],
    ) {
        if self.editor == AgentEditor::Closed {
            return;
        }
        if !self.editor_dialog.is_visible() {
            self.editor_dialog.show();
        }
        let editor = self.editor;
        let buffer = self.buffer.clone();
        let focused = self.dialog_focus.current().copied();
        let title = match editor {
            AgentEditor::Spawn => text(Text::DelegateProfile),
            AgentEditor::Message => text(Text::MessageRunningAgent),
            AgentEditor::Closed => text(Text::Agent),
        };
        self.profile_picker.set_total(fleet.profiles.profiles.len());
        self.profile_picker.ensure_visible(10);
        self.dependency_picker.set_total(fleet.agents.len());
        self.dependency_picker.ensure_visible(8);
        self.claim_picker.set_total(workspace_files.len());
        self.claim_picker.ensure_visible(8);
        let profile_picker = self.profile_picker.clone();
        let dependency_picker = self.dependency_picker.clone();
        let claim_picker = self.claim_picker.clone();
        let profiles = Arc::clone(&fleet.profiles.profiles);
        let agents = Arc::clone(&fleet.agents);
        let workspace_files = workspace_files.to_vec();
        let selected_dependencies = self.selected_dependencies.clone();
        let selected_file_claims = self.selected_file_claims.clone();
        let profile_diagnostics = fleet.profiles.diagnostics.len();
        let selected_profile_id = self.selected_profile_id.clone();
        let writer_selected = selected_profile_id.as_ref().is_some_and(|id| {
            profiles.iter().any(|profile| {
                &profile.id == id && profile.mode == crate::agent::SubagentMode::Writer
            })
        });
        let config = DialogConfig::new(title)
            .width_percent(90)
            .height_percent(if editor == AgentEditor::Spawn { 86 } else { 48 })
            .min_size(60, if editor == AgentEditor::Spawn { 30 } else { 14 })
            .max_size(175, if editor == AgentEditor::Spawn { 58 } else { 34 })
            .border_color(Color::Cyan)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.editor_dialog, |frame, area, _| {
            let rows = if editor == AgentEditor::Spawn {
                Layout::vertical([
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Min(7),
                    Constraint::Length(3),
                ])
                .split(area)
            } else {
                Layout::vertical([
                    Constraint::Length(0),
                    Constraint::Length(0),
                    Constraint::Min(7),
                    Constraint::Length(3),
                ])
                .split(area)
            };
            if editor == AgentEditor::Spawn {
                let columns =
                    Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
                        .split(rows[0]);
                let items = profiles
                    .iter()
                    .map(|profile| {
                        format!(
                            "{}  [{} · {}]",
                            sanitize_for_display(&profile.name),
                            localized_profile_source(profile.source),
                            localized_agent_mode(profile.mode)
                        )
                    })
                    .collect::<Vec<_>>();
                let profile_block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(
                        " {} · {} {} · {profile_diagnostics} {} ",
                        text(Text::ProfileLabel),
                        profiles.len(),
                        text(Text::LoadedLabel),
                        text(Text::WarningLabel),
                    ))
                    .border_style(Style::default().fg(
                        if focused == Some(AgentDialogFocus::Profiles) {
                            Color::LightCyan
                        } else {
                            Color::DarkGray
                        },
                    ));
                let profile_inner = profile_block.inner(columns[0]);
                frame.render_widget(profile_block, columns[0]);
                frame.render_widget(
                    ListPicker::new(&items, &profile_picker)
                        .style(ListPickerStyle::bracket().bordered(false)),
                    profile_inner,
                );
                for row in 0..usize::from(profile_inner.height) {
                    let index = usize::from(profile_picker.scroll).saturating_add(row);
                    if index >= profiles.len() {
                        break;
                    }
                    clicks.register(
                        Rect::new(
                            profile_inner.x,
                            profile_inner.y.saturating_add(row as u16),
                            profile_inner.width,
                            1,
                        ),
                        AgentHit::Profile(index),
                    );
                }
                let profile = selected_profile_id
                    .as_ref()
                    .and_then(|id| profiles.iter().find(|profile| &profile.id == id));
                frame.render_widget(
                    profile_summary(profile, fleet.profiles.diagnostics.first()),
                    columns[1],
                );
            }
            if editor == AgentEditor::Spawn {
                let columns =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(rows[1]);
                let dependency_items = agents
                    .iter()
                    .map(|agent| {
                        format!(
                            "[{}] {}  {}  {}",
                            if selected_dependencies.contains(&agent.id) {
                                "x"
                            } else {
                                " "
                            },
                            agent.id,
                            localized_agent_status(agent.status),
                            sanitize_for_display(&agent.label)
                        )
                    })
                    .collect::<Vec<_>>();
                let dependency_block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::DependsClickSpace)))
                    .border_style(Style::default().fg(
                        if focused == Some(AgentDialogFocus::Dependencies) {
                            Color::LightCyan
                        } else {
                            Color::DarkGray
                        },
                    ));
                let dependency_inner = dependency_block.inner(columns[0]);
                frame.render_widget(dependency_block, columns[0]);
                if dependency_items.is_empty() {
                    frame.render_widget(
                        Paragraph::new(text(Text::NoPredecessorAgents))
                            .style(Style::default().fg(Color::DarkGray)),
                        dependency_inner,
                    );
                } else {
                    frame.render_widget(
                        ListPicker::new(&dependency_items, &dependency_picker)
                            .style(ListPickerStyle::bracket().bordered(false)),
                        dependency_inner,
                    );
                    for row in 0..usize::from(dependency_inner.height) {
                        let index = usize::from(dependency_picker.scroll).saturating_add(row);
                        if index >= agents.len() {
                            break;
                        }
                        clicks.register(
                            Rect::new(
                                dependency_inner.x,
                                dependency_inner.y.saturating_add(row as u16),
                                dependency_inner.width,
                                1,
                            ),
                            AgentHit::Dependency(index),
                        );
                    }
                }

                let claim_items = workspace_files
                    .iter()
                    .map(|path| {
                        format!(
                            "[{}] {}",
                            if selected_file_claims.contains(path) {
                                "x"
                            } else {
                                " "
                            },
                            sanitize_for_display(path)
                        )
                    })
                    .collect::<Vec<_>>();
                let claim_title = if writer_selected {
                    text(Text::WriterClaimsExclusive)
                } else {
                    text(Text::FileClaimsChooseWriter)
                };
                let claim_block = Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {claim_title} "))
                    .border_style(Style::default().fg(
                        if focused == Some(AgentDialogFocus::Claims) && writer_selected {
                            Color::LightCyan
                        } else {
                            Color::DarkGray
                        },
                    ));
                let claim_inner = claim_block.inner(columns[1]);
                frame.render_widget(claim_block, columns[1]);
                if claim_items.is_empty() {
                    frame.render_widget(
                        Paragraph::new(text(Text::NoIndexedProjectFiles))
                            .style(Style::default().fg(Color::DarkGray)),
                        claim_inner,
                    );
                } else {
                    frame.render_widget(
                        ListPicker::new(&claim_items, &claim_picker)
                            .style(ListPickerStyle::bracket().bordered(false)),
                        claim_inner,
                    );
                    if writer_selected {
                        for row in 0..usize::from(claim_inner.height) {
                            let index = usize::from(claim_picker.scroll).saturating_add(row);
                            if index >= workspace_files.len() {
                                break;
                            }
                            clicks.register(
                                Rect::new(
                                    claim_inner.x,
                                    claim_inner.y.saturating_add(row as u16),
                                    claim_inner.width,
                                    1,
                                ),
                                AgentHit::Claim(index),
                            );
                        }
                    }
                }
            }
            let target = selected.map_or_else(
                || text(Text::NewSubagent).to_owned(),
                |agent| format!("{} ({})", agent.id, sanitize_for_display(&agent.label)),
            );
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("{}: {target}", text(Text::TargetLabel))),
                    Line::from(""),
                    Line::from(if buffer.is_empty() {
                        text(Text::TypeTaskFollowUp).to_owned()
                    } else {
                        sanitize_for_display(&buffer)
                    }),
                    Line::from(""),
                    Line::from(Span::styled(
                        text(Text::AgentEditorHelp),
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {} ", text(Text::MessageLabel))),
                )
                .wrap(Wrap { trim: false }),
                rows[2],
            );
            clicks.register(rows[2], AgentHit::Dialog(AgentDialogFocus::Task));
            draw_dialog_buttons(
                frame,
                rows[3],
                text(Text::Cancel),
                text(Text::SendLabel),
                focused,
                has_visible_text(&buffer)
                    && (editor != AgentEditor::Spawn || selected_profile_id.is_some()),
                clicks,
            );
        });
        popup.render(frame);
    }

    pub fn draw_command_approval(&mut self, frame: &mut Frame<'_>, agent: &SubagentSnapshot) {
        let Some(pending) = &agent.pending_command else {
            return;
        };
        if !self.command_dialog.is_visible() {
            self.command_focus.set(AgentDecisionFocus::Decline);
            self.command_dialog.show();
        }
        let focused = self.command_focus.current().copied();
        let command = truncate_for_display(&sanitize_for_display(&pending.command), DETAIL_LIMIT);
        let capability = if pending.mcp {
            text(Text::McpToolNoun)
        } else {
            text(Text::ShellCommandNoun)
        };
        let config = DialogConfig::new(format!(
            "{} {} {capability}",
            agent.id,
            text(Text::RequestsLabel)
        ))
        .width_percent(86)
        .height_percent(52)
        .min_size(60, 15)
        .max_size(170, 40)
        .border_color(Color::Yellow)
        .focused_border_color(Color::LightYellow)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.command_dialog, |frame, area, _| {
            let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(3)]).split(area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        format!(
                            "{} • {} / {}",
                            sanitize_for_display(&agent.label),
                            sanitize_for_display(&agent.deployment),
                            agent.reasoning_effort
                        ),
                        Style::default()
                            .fg(Color::LightCyan)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(command.clone()),
                    Line::from(""),
                    Line::from(Span::styled(
                        if pending.mcp {
                            text(Text::McpPermissionSafeDefault)
                        } else {
                            text(Text::ShellWorktreeSafeDefault)
                        },
                        Style::default().fg(Color::DarkGray),
                    )),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(if pending.mcp {
                            format!(" {} ", text(Text::SanitizedMcpRequest))
                        } else {
                            format!(" {} ", text(Text::SanitizedCommand))
                        }),
                )
                .wrap(Wrap { trim: false }),
                rows[0],
            );
            draw_decision_buttons(
                frame,
                rows[1],
                text(Text::DeclineEsc),
                if pending.mcp {
                    text(Text::AllowOnce)
                } else {
                    text(Text::RunCommand)
                },
                focused,
                true,
                AgentHit::Command,
                clicks,
            );
        });
        popup.render(frame);
    }

    pub fn hide_command_dialog(&mut self) {
        self.command_dialog.hide();
    }

    pub fn draw_binary_review(&mut self, frame: &mut Frame<'_>, review: &SubagentFileReview) {
        if !self.binary_dialog.is_visible() {
            self.binary_focus.set(AgentDecisionFocus::Decline);
            self.binary_dialog.show();
        }
        let focused = self.binary_focus.current().copied();
        let path = sanitize_for_display(&review.path);
        let config = DialogConfig::new(text(Text::ReviewBinaryFile))
            .width_percent(72)
            .height_percent(38)
            .min_size(54, 12)
            .max_size(130, 26)
            .border_color(Color::Yellow)
            .focused_border_color(Color::LightYellow)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.binary_dialog, |frame, area, _| {
            let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(3)]).split(area);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        path.clone(),
                        Style::default().fg(Color::LightCyan),
                    )),
                    Line::from(""),
                    Line::from(text(Text::BinaryNoHunkReview)),
                    Line::from(text(Text::ApproveBinaryCas)),
                ])
                .block(Block::default().borders(Borders::ALL))
                .wrap(Wrap { trim: false }),
                rows[0],
            );
            draw_decision_buttons(
                frame,
                rows[1],
                text(Text::RejectFile),
                text(Text::ApproveWholeFile),
                focused,
                true,
                AgentHit::Binary,
                clicks,
            );
        });
        popup.render(frame);
    }

    pub fn hide_binary_dialog(&mut self) {
        self.binary_dialog.hide();
    }

    fn animation_glyph(&self) -> &'static str {
        let frame =
            self.animation_started.elapsed().as_millis() / ANIMATION_FRAME.as_millis().max(1);
        match frame % 4 {
            0 => "·",
            1 => "•",
            2 => "●",
            _ => "•",
        }
    }
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(1);
    let mut width = 0_usize;
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let next = UnicodeWidthStr::width(grapheme);
        if width.saturating_add(next) > content_width {
            break;
        }
        output.push_str(grapheme);
        width = width.saturating_add(next);
    }
    output.push('…');
    output
}

impl Default for AgentUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_decision_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    decline_label: &str,
    approve_label: &str,
    focused: Option<AgentDecisionFocus>,
    approve_enabled: bool,
    hit: impl Fn(AgentDecisionFocus) -> AgentHit,
    clicks: &mut ClickRegionRegistry<AgentHit>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    let decline_focused = focused == Some(AgentDecisionFocus::Decline);
    let approve_focused = focused == Some(AgentDecisionFocus::Approve);
    draw_decision_button(
        frame,
        columns[0],
        decline_label,
        decline_focused,
        true,
        ButtonStyle::danger(),
        hit(AgentDecisionFocus::Decline),
        clicks,
    );
    draw_decision_button(
        frame,
        columns[1],
        approve_label,
        approve_focused,
        approve_enabled,
        ButtonStyle::success(),
        hit(AgentDecisionFocus::Approve),
        clicks,
    );
}

fn draw_dialog_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    cancel_label: &str,
    submit_label: &str,
    focused: Option<AgentDialogFocus>,
    submit_enabled: bool,
    clicks: &mut ClickRegionRegistry<AgentHit>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    draw_decision_button(
        frame,
        columns[0],
        cancel_label,
        focused == Some(AgentDialogFocus::Cancel),
        true,
        ButtonStyle::danger(),
        AgentHit::Dialog(AgentDialogFocus::Cancel),
        clicks,
    );
    draw_decision_button(
        frame,
        columns[1],
        submit_label,
        focused == Some(AgentDialogFocus::Submit),
        submit_enabled,
        ButtonStyle::success(),
        AgentHit::Dialog(AgentDialogFocus::Submit),
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_decision_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    enabled: bool,
    style: ButtonStyle,
    hit: AgentHit,
    clicks: &mut ClickRegionRegistry<AgentHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let button = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style);
    let region = button.render_stateful(area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, hit);
    }
}

fn profile_summary(
    profile: Option<&AgentProfileSummary>,
    diagnostic: Option<&String>,
) -> Paragraph<'static> {
    let Some(profile) = profile else {
        return Paragraph::new(text(Text::NoProfileAvailable))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::ProfileDetails))),
            )
            .wrap(Wrap { trim: false });
    };
    let tools = profile
        .allowed_tools
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let runtime = match (&profile.deployment, profile.reasoning_effort) {
        (Some(deployment), Some(effort)) => {
            format!("{} / {effort}", sanitize_for_display(deployment))
        }
        (Some(deployment), None) => {
            format!(
                "{} / {}",
                sanitize_for_display(deployment),
                text(Text::InheritEffort)
            )
        }
        (None, Some(effort)) => format!("{} / {effort}", text(Text::InheritModel)),
        (None, None) => text(Text::InheritCurrentRuntime).to_owned(),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            sanitize_for_display(&profile.name),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{} · {} · {}",
            sanitize_for_display(&profile.id),
            localized_profile_source(profile.source),
            localized_agent_mode(profile.mode)
        )),
        Line::from(format!("{}: {runtime}", text(Text::Runtime))),
        Line::from(format!(
            "{}: {}",
            text(Text::ToolsLabel),
            sanitize_for_display(&tools)
        )),
        Line::from(""),
        Line::from(sanitize_for_display(&profile.description)),
    ];
    if let Some(diagnostic) = diagnostic {
        lines.push(Line::from(Span::styled(
            format!(
                "{}: {}",
                text(Text::WarningLabel),
                truncate_for_display(&sanitize_for_display(diagnostic), 180)
            ),
            Style::default().fg(Color::LightYellow),
        )));
    }
    Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", text(Text::ProfileDetails))),
        )
        .wrap(Wrap { trim: false })
}

fn localized_agent_mode(mode: SubagentMode) -> &'static str {
    match mode {
        SubagentMode::Research => text(Text::ReadOnlyAgentMode),
        SubagentMode::Writer => text(Text::IsolatedWriterMode),
    }
}

fn localized_profile_source(source: AgentProfileSource) -> &'static str {
    match source {
        AgentProfileSource::BuiltIn => text(Text::BuiltInSource),
        AgentProfileSource::User => text(Text::UserSource),
        AgentProfileSource::Project => text(Text::ProjectSource),
    }
}

fn localized_agent_status(status: SubagentStatus) -> &'static str {
    match status {
        SubagentStatus::Queued => text(Text::QueueLabel),
        SubagentStatus::WaitingDependencies => text(Text::WaitingDependenciesStatus),
        SubagentStatus::Starting => text(Text::StartingStatus),
        SubagentStatus::Running => text(Text::RunningStatus),
        SubagentStatus::WaitingApproval => text(Text::WaitingApprovalStatus),
        SubagentStatus::WaitingBudget => text(Text::TokenGuardReached),
        SubagentStatus::Cancelling => text(Text::StoppingStatus),
        SubagentStatus::RecoveryRequired => text(Text::RecoveryRequiredStatus),
        SubagentStatus::ReadyForReview => text(Text::ReadyForReviewStatus),
        SubagentStatus::Completed => text(Text::Completed),
        SubagentStatus::Failed | SubagentStatus::DependencyFailed => text(Text::Failed),
        SubagentStatus::Cancelled => text(Text::Cancelled),
        SubagentStatus::TimedOut => text(Text::TimedOutStatus),
        SubagentStatus::Interrupted => text(Text::Interrupted),
    }
}

fn localized_agent_message(message: &str) -> String {
    let message = sanitize_for_display(message);
    let exact = match message.as_str() {
        "Waiting for an execution slot" => Some(text(Text::QueueLabel)),
        "All isolated changes were reviewed" => Some(text(Text::ReviewedLabel)),
        "Interrupted by application restart" => Some(text(Text::DeliveryInterruptedRestart)),
        "Sub-agent completed" => Some(text(Text::Completed)),
        "Sub-agent was cancelled" => Some(text(Text::Cancelled)),
        "Preparing isolated runtime" => Some(text(Text::StartingStatus)),
        "Cancellation requested" | "Cancellation requested after the active mutation" => {
            Some(text(Text::StoppingStatus))
        }
        "Ancestor cancellation cascaded to this child" => Some(text(Text::StoppingStatus)),
        "User stopped the branch at its token budget" => Some(text(Text::Cancelled)),
        "Recovery abandoned; isolated file changes remain available for review" => {
            Some(text(Text::Cancelled))
        }
        "Writer recovery is pending" => Some(text(Text::RecoveryRequiredStatus)),
        _ => None,
    };
    if let Some(value) = exact {
        return value.to_owned();
    }
    if let Some(rest) = message.strip_prefix("Waiting for dependencies:") {
        return format!("{}: {}", text(Text::WaitingDependenciesStatus), rest.trim());
    }
    if let Some(rest) = message.strip_prefix("Waiting for writer file claims held by ") {
        return format!("{}: {rest}", text(Text::WaitingDependenciesStatus));
    }
    if let Some(rest) = message.strip_prefix("Recovery attempt ") {
        let attempt = rest.split_whitespace().next().unwrap_or_default();
        return format!("{} {attempt}", text(Text::RecoveryAttemptCheckpoint));
    }
    if let Some(rest) = message.strip_prefix("Running ") {
        return format!("{}: {rest}", text(Text::RunningStatus));
    }
    if message.starts_with("Token budget ") {
        return text(Text::TokenGuardReached).to_owned();
    }
    if let Some(rest) = message.strip_prefix("Task exceeded ") {
        return format!("{}: {rest}", text(Text::TimedOutStatus));
    }
    if let Some(rest) = message.strip_prefix("Writer stopped before completion:") {
        return format!("{}: {}", text(Text::Failed), rest.trim());
    }
    if message.starts_with("Writer stopped before a durable terminal result")
        || message.starts_with("Application stopped the writer at a durable recovery boundary")
    {
        return text(Text::RecoveryRequiredStatus).to_owned();
    }
    message
}

fn status_color(agent: &SubagentSnapshot) -> Color {
    match agent.status {
        SubagentStatus::Queued | SubagentStatus::WaitingDependencies | SubagentStatus::Starting => {
            Color::Yellow
        }
        SubagentStatus::Running
        | SubagentStatus::WaitingApproval
        | SubagentStatus::WaitingBudget => Color::LightCyan,
        SubagentStatus::Cancelling => Color::LightYellow,
        SubagentStatus::RecoveryRequired => Color::LightYellow,
        SubagentStatus::ReadyForReview => Color::LightMagenta,
        SubagentStatus::Completed => Color::LightGreen,
        SubagentStatus::Failed | SubagentStatus::TimedOut | SubagentStatus::DependencyFailed => {
            Color::LightRed
        }
        SubagentStatus::Cancelled | SubagentStatus::Interrupted => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{AgentBrowseFocus, AgentDialogFocus, AgentEditor, AgentHit, AgentUiState};
    use crate::{
        agent::{
            AgentProfileCatalog, SubagentBudgetScope, SubagentFleetSnapshot, SubagentId,
            SubagentMode, SubagentPendingBudget, SubagentPendingCommand, SubagentRecoverySummary,
            SubagentSnapshot, SubagentStatus,
        },
        api::ReasoningEffort,
    };

    #[test]
    fn editor_is_explicit_and_clears_text_on_close() {
        let mut state = AgentUiState::new();
        let fleet = fleet(Vec::new());
        state.open_spawn(&fleet);
        state.push('x');
        assert_eq!(state.editor(), AgentEditor::Spawn);
        assert_eq!(state.selected_profile_id(), Some("builtin:research"));
        assert_eq!(state.buffer(), "x");
        state.close_editor();
        assert_eq!(state.editor(), AgentEditor::Closed);
        assert!(state.buffer().is_empty());
    }

    #[test]
    fn editor_backspace_removes_one_grapheme() {
        let mut state = AgentUiState::new();
        state.open_message();
        for character in "task 👩‍💻".chars() {
            state.push(character);
        }

        state.pop();

        assert_eq!(state.buffer(), "task ");
    }

    #[test]
    fn message_editor_focus_skips_spawn_only_controls() {
        let mut state = AgentUiState::new();
        state.open_message();

        state.next_focus();
        assert_eq!(state.dialog_focused(), Some(AgentDialogFocus::Task));
        state.previous_focus();
        assert_eq!(state.dialog_focused(), Some(AgentDialogFocus::Submit));
    }

    #[test]
    fn invisible_editor_text_does_not_enable_submit() -> Result<(), Box<dyn std::error::Error>> {
        let fleet = fleet(Vec::new());
        let mut state = AgentUiState::new();
        state.open_message();
        state.push('\u{200b}');
        state.push('\u{200d}');
        state.begin_frame();
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        terminal.draw(|frame| state.draw_editor(frame, &fleet, None, &[]))?;

        assert!(!has_hit(
            &state,
            AgentHit::Dialog(AgentDialogFocus::Submit),
            100,
            30,
        ));
        Ok(())
    }

    #[test]
    fn command_approval_sanitizes_agent_metadata() -> Result<(), Box<dyn std::error::Error>> {
        let mut agent = snapshot(4, "bad\u{1b}[2Jlabel");
        agent.deployment = "model\u{202e}evil".to_owned();
        agent.pending_command = Some(SubagentPendingCommand {
            action_id: 1,
            command: "cargo check".to_owned(),
            model_requested_confirmation: false,
            mcp: false,
        });
        let mut state = AgentUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        terminal.draw(|frame| state.draw_command_approval(frame, &agent))?;

        let buffer = terminal.backend().buffer();
        let list_width = 46;
        let rendered = (0..buffer.area.height)
            .flat_map(|row| (0..list_width).map(move |column| buffer[(column, row)].symbol()))
            .collect::<String>();
        assert!(rendered.contains("bad\\x1b[2Jlabel"));
        assert!(rendered.contains("model<U+202E>evil"));
        Ok(())
    }

    #[test]
    fn agent_detail_does_not_hide_a_late_cleanup_error() -> Result<(), Box<dyn std::error::Error>> {
        let mut agent = snapshot(5, "writer");
        agent.result = "implementation finished".to_owned();
        agent.error = Some("worktree cleanup failed".to_owned());
        let fleet = fleet(vec![agent]);
        let mut state = AgentUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .flat_map(|row| {
                (0..buffer.area.width).map(move |column| buffer[(column, row)].symbol())
            })
            .collect::<String>();
        assert!(rendered.contains("worktree cleanup failed"));
        Ok(())
    }

    #[test]
    fn clickable_actions_exist_and_disabled_actions_have_no_hit_region()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = AgentUiState::new();
        let fleet = SubagentFleetSnapshot {
            revision: 1,
            enabled: true,
            capacity: 4,
            active: 0,
            total_tokens: 0,
            token_budget: 500_000,
            availability_error: None,
            mcp_enabled: false,
            mcp_status: crate::notice::UiNotice::SubagentMcpDisabled,
            profiles: Default::default(),
            agents: Arc::from([]),
        };
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        let mut new_agent = false;
        let mut reload = false;
        let mut disabled_stop = false;
        for row in 0..30 {
            for column in 0..100 {
                match state.clicked(column, row) {
                    Some(AgentHit::Browse(AgentBrowseFocus::New)) => new_agent = true,
                    Some(AgentHit::Browse(AgentBrowseFocus::Reload)) => reload = true,
                    Some(AgentHit::Browse(AgentBrowseFocus::Stop)) => disabled_stop = true,
                    _ => {}
                }
            }
        }
        assert!(new_agent);
        assert!(reload);
        assert!(!disabled_stop);
        Ok(())
    }

    #[test]
    fn recovery_actions_are_clickable_and_resume_fails_closed_without_a_worktree()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut agent = snapshot(8, "recover writer");
        agent.mode = SubagentMode::Writer;
        agent.status = SubagentStatus::RecoveryRequired;
        agent.recovery = Some(SubagentRecoverySummary {
            attempt: 1,
            checkpoint_at: Utc::now(),
            reason: "application restarted".to_owned(),
            uncertain_action: None,
            can_resume: true,
        });
        let mut state = AgentUiState::new();
        let mut fleet = fleet(vec![agent]);
        let backend = TestBackend::new(110, 36);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        assert!(has_browse_hit(&state, AgentBrowseFocus::Resume, 110, 36));
        assert!(has_browse_hit(&state, AgentBrowseFocus::Abandon, 110, 36));
        assert!(!has_browse_hit(&state, AgentBrowseFocus::Stop, 110, 36));

        if let Some(recovery) = Arc::make_mut(&mut fleet.agents)[0].recovery.as_mut() {
            recovery.can_resume = false;
        }
        state.begin_frame();
        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;
        assert!(!has_browse_hit(&state, AgentBrowseFocus::Resume, 110, 36));
        assert!(has_browse_hit(&state, AgentBrowseFocus::Abandon, 110, 36));
        Ok(())
    }

    #[test]
    fn exhausted_budget_exposes_mouse_and_keyboard_decisions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut agent = snapshot(9, "budget guard");
        agent.status = SubagentStatus::WaitingBudget;
        agent.pending_budget = Some(SubagentPendingBudget {
            scope: SubagentBudgetScope::SessionTree,
            used: 500_000,
            limit: 500_000,
            suggested_increase: 50_000,
        });
        let fleet = fleet(vec![agent]);
        let mut state = AgentUiState::new();
        let backend = TestBackend::new(110, 36);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        assert!(has_browse_hit(
            &state,
            AgentBrowseFocus::RaiseBudget,
            110,
            36
        ));
        assert!(has_browse_hit(
            &state,
            AgentBrowseFocus::StopAtBudget,
            110,
            36
        ));
        assert!(!has_browse_hit(&state, AgentBrowseFocus::Resume, 110, 36));
        state.focus_browse(AgentBrowseFocus::RaiseBudget);
        assert_eq!(state.browse_focused(), Some(AgentBrowseFocus::RaiseBudget));
        Ok(())
    }

    #[test]
    fn selected_agent_uses_stable_id_after_reordering() {
        let mut state = AgentUiState::new();
        let first = snapshot(1, "first");
        let second = snapshot(2, "second");
        let mut fleet = fleet(vec![first.clone(), second.clone()]);
        state.sync(&fleet);
        state.select_index(&fleet, 1);
        assert_eq!(state.selected_id(), Some(SubagentId::new(2)));

        fleet.agents = Arc::from([second, first]);
        state.sync(&fleet);
        assert_eq!(state.selected_id(), Some(SubagentId::new(2)));
    }

    #[test]
    fn selected_agent_stays_visible_when_labels_are_long() -> Result<(), Box<dyn std::error::Error>>
    {
        let agents = (1..=50)
            .map(|id| {
                snapshot(
                    id,
                    &format!("delegated task {id}\nsecond line that must not consume a list row"),
                )
            })
            .collect::<Vec<_>>();
        let fleet = fleet(agents);
        let mut state = AgentUiState::new();
        state.select_index(&fleet, 49);
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;

        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        let buffer = terminal.backend().buffer();
        let rendered = (0..buffer.area.height)
            .flat_map(|row| {
                (0..buffer.area.width).map(move |column| buffer[(column, row)].symbol())
            })
            .collect::<String>();
        assert!(rendered.contains("agent-0050"));
        assert!(has_hit(&state, AgentHit::Item(49), 120, 36));
        Ok(())
    }

    #[test]
    fn sequential_navigation_keeps_the_selected_agent_visible()
    -> Result<(), Box<dyn std::error::Error>> {
        let agents = (1..=50)
            .map(|id| {
                snapshot(
                    id,
                    &format!("delegated task {id} {}", "wide label ".repeat(8)),
                )
            })
            .collect::<Vec<_>>();
        let fleet = fleet(agents);
        let mut state = AgentUiState::new();
        state.sync(&fleet);
        for _ in 0..20 {
            state.next_item(&fleet);
        }
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;

        terminal.draw(|frame| state.draw_tab(frame, frame.area(), &fleet, true))?;

        let buffer = terminal.backend().buffer();
        let list = (0..buffer.area.height)
            .map(|row| {
                (0..47)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(list.contains("> agent-0021"), "{list}");
        Ok(())
    }

    #[test]
    fn spawn_dialog_exposes_clickable_profile_rows() -> Result<(), Box<dyn std::error::Error>> {
        let fleet = fleet(Vec::new());
        let mut state = AgentUiState::new();
        state.open_spawn(&fleet);
        state.begin_frame();
        let backend = TestBackend::new(130, 42);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw_editor(frame, &fleet, None, &[]))?;

        let mut second_profile = None;
        for row in 0..42 {
            for column in 0..130 {
                if state.clicked(column, row) == Some(AgentHit::Profile(1)) {
                    second_profile = Some(1);
                }
            }
        }
        assert_eq!(second_profile, Some(1));
        state.select_profile(&fleet, 1);
        assert_eq!(state.selected_profile_id(), Some("builtin:writer"));
        Ok(())
    }

    #[test]
    fn dag_dependencies_and_writer_claims_are_clickable_multi_selectors()
    -> Result<(), Box<dyn std::error::Error>> {
        let predecessor = snapshot(7, "prepare parser contract");
        let fleet = fleet(vec![predecessor]);
        let files = vec!["src/parser.rs".to_owned(), "tests/parser.rs".to_owned()];
        let mut state = AgentUiState::new();
        state.open_spawn(&fleet);
        state.select_profile(&fleet, 1);
        state.begin_frame();
        let backend = TestBackend::new(140, 46);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw_editor(frame, &fleet, None, &files))?;

        assert!(has_hit(&state, AgentHit::Dependency(0), 140, 46));
        assert!(has_hit(&state, AgentHit::Claim(0), 140, 46));
        state.toggle_dependency(&fleet, 0);
        state.toggle_claim(&fleet, &files, 0);
        assert_eq!(state.selected_dependencies(), vec![SubagentId::new(7)]);
        assert_eq!(state.selected_file_claims(), vec!["src/parser.rs"]);
        Ok(())
    }

    fn fleet(agents: Vec<SubagentSnapshot>) -> SubagentFleetSnapshot {
        SubagentFleetSnapshot {
            revision: 1,
            enabled: true,
            capacity: 4,
            active: 0,
            total_tokens: 0,
            token_budget: 500_000,
            availability_error: None,
            mcp_enabled: false,
            mcp_status: crate::notice::UiNotice::SubagentMcpDisabled,
            profiles: test_profiles(),
            agents: Arc::from(agents),
        }
    }

    fn test_profiles() -> crate::agent::AgentProfileCatalogSnapshot {
        let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        AgentProfileCatalog::load(workspace).snapshot()
    }

    fn snapshot(id: u64, label: &str) -> SubagentSnapshot {
        let now = Utc::now();
        SubagentSnapshot {
            id: SubagentId::new(id),
            parent_id: None,
            depth: 1,
            revision: 1,
            session_id: None,
            label: label.to_owned(),
            task: "task".to_owned(),
            profile_id: "builtin:research".to_owned(),
            profile_name: "Research".to_owned(),
            mode: SubagentMode::Research,
            status: SubagentStatus::Completed,
            deployment: "model".to_owned(),
            reasoning_effort: ReasoningEffort::High,
            created_at: now,
            started_at: Some(now),
            completed_at: Some(now),
            updated_at: now,
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            token_budget: 150_000,
            tool_iterations: 0,
            last_message: "done".to_owned(),
            result: "done".to_owned(),
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

    fn has_browse_hit(
        state: &AgentUiState,
        focus: AgentBrowseFocus,
        width: u16,
        height: u16,
    ) -> bool {
        has_hit(state, AgentHit::Browse(focus), width, height)
    }

    fn has_hit(state: &AgentUiState, hit: AgentHit, width: u16, height: u16) -> bool {
        (0..height).any(|row| (0..width).any(|column| state.clicked(column, row) == Some(hit)))
    }
}
