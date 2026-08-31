use std::sync::Arc;
use std::time::{Duration, Instant};

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
        Button, ButtonState, ButtonStyle, ButtonVariant, CheckBox, CheckBoxState, CheckBoxStyle,
        DialogConfig, DialogState, PopupDialog,
    },
    state::FocusManager,
};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::agent::{GoalStatus, PlanReview, WorkModes, side_chat::has_visible_text, state::TurnId};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Clone, Copy)]
struct ControlState {
    focused: bool,
    enabled: bool,
}

pub(crate) fn goal_status_label(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => text(Text::ActiveStatus),
        GoalStatus::Completed => text(Text::Completed),
        GoalStatus::Blocked => text(Text::BlockedStatus),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModesFocus {
    Close,
    Plan,
    Explore,
    Review,
    Goal,
    Deep,
    EditGoal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoalEditorFocus {
    Text,
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModesHit {
    Close,
    Plan,
    Explore,
    Review,
    Goal,
    Deep,
    EditGoal,
    GoalText,
    SaveGoal,
    CancelGoal,
}

#[derive(Debug, Clone)]
pub struct ModesUiState {
    open: bool,
    editing_goal: bool,
    dialog: DialogState<()>,
    overview_focus: FocusManager<ModesFocus>,
    editor_focus: FocusManager<GoalEditorFocus>,
    clicks: ClickRegionRegistry<ModesHit>,
    goal_buffer: String,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl ModesUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut overview_focus = FocusManager::new();
        for focus in [
            ModesFocus::Close,
            ModesFocus::Plan,
            ModesFocus::Explore,
            ModesFocus::Review,
            ModesFocus::Goal,
            ModesFocus::Deep,
            ModesFocus::EditGoal,
        ] {
            overview_focus.register(focus);
        }
        overview_focus.set(ModesFocus::Plan);
        let mut editor_focus = FocusManager::new();
        for focus in [
            GoalEditorFocus::Text,
            GoalEditorFocus::Save,
            GoalEditorFocus::Cancel,
        ] {
            editor_focus.register(focus);
        }
        editor_focus.set(GoalEditorFocus::Text);
        Self {
            open: false,
            editing_goal: false,
            dialog: DialogState::new(()),
            overview_focus,
            editor_focus,
            clicks: ClickRegionRegistry::new(),
            goal_buffer: String::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    #[must_use]
    pub const fn is_editing_goal(&self) -> bool {
        self.editing_goal
    }

    pub fn open(&mut self, modes: &WorkModes) {
        self.open = true;
        self.editing_goal = false;
        self.goal_buffer = modes
            .goal
            .as_ref()
            .map_or_else(String::new, |goal| goal.objective.clone());
        self.overview_focus.set(ModesFocus::Plan);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.editing_goal = false;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn tick(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_animation_at) >= ANIMATION_STEP {
            self.animation_frame = self.animation_frame.wrapping_add(1);
            self.last_animation_at = now;
        }
    }

    pub fn edit_goal(&mut self, modes: &WorkModes) {
        self.goal_buffer = modes
            .goal
            .as_ref()
            .map_or_else(String::new, |goal| goal.objective.clone());
        self.editing_goal = true;
        self.editor_focus.set(GoalEditorFocus::Text);
    }

    pub fn cancel_goal_edit(&mut self) {
        self.editing_goal = false;
        self.overview_focus.set(ModesFocus::Goal);
    }

    #[must_use]
    pub fn goal_buffer(&self) -> &str {
        &self.goal_buffer
    }

    #[must_use]
    pub fn goal_has_visible_text(&self) -> bool {
        has_visible_text(&self.goal_buffer)
    }

    pub fn push_goal_char(&mut self, character: char) {
        if !character.is_control()
            && self.goal_buffer.len().saturating_add(character.len_utf8())
                <= crate::agent::modes::MAX_GOAL_BYTES
        {
            self.goal_buffer.push(character);
        }
    }

    pub fn push_goal_text(&mut self, text: &str) {
        for character in text.chars() {
            if self.goal_buffer.len().saturating_add(character.len_utf8())
                > crate::agent::modes::MAX_GOAL_BYTES
            {
                break;
            }
            if character == '\n' || character == '\t' || !character.is_control() {
                self.goal_buffer.push(character);
            }
        }
    }

    pub fn pop_goal_char(&mut self) {
        if let Some((index, _)) = self.goal_buffer.grapheme_indices(true).next_back() {
            self.goal_buffer.truncate(index);
        }
    }

    pub fn goal_newline(&mut self) {
        if self.goal_buffer.len() < crate::agent::modes::MAX_GOAL_BYTES {
            self.goal_buffer.push('\n');
        }
    }

    pub fn next_focus(&mut self) {
        if self.editing_goal {
            self.editor_focus.next();
        } else {
            self.overview_focus.next();
        }
    }

    pub fn previous_focus(&mut self) {
        if self.editing_goal {
            self.editor_focus.prev();
        } else {
            self.overview_focus.prev();
        }
    }

    #[must_use]
    pub fn overview_focused(&self) -> Option<ModesFocus> {
        self.overview_focus.current().copied()
    }

    #[must_use]
    pub fn editor_focused(&self) -> Option<GoalEditorFocus> {
        self.editor_focus.current().copied()
    }

    pub fn focus_hit(&mut self, hit: ModesHit) {
        match hit {
            ModesHit::Close => self.overview_focus.set(ModesFocus::Close),
            ModesHit::Plan => self.overview_focus.set(ModesFocus::Plan),
            ModesHit::Explore => self.overview_focus.set(ModesFocus::Explore),
            ModesHit::Review => self.overview_focus.set(ModesFocus::Review),
            ModesHit::Goal => self.overview_focus.set(ModesFocus::Goal),
            ModesHit::Deep => self.overview_focus.set(ModesFocus::Deep),
            ModesHit::EditGoal => self.overview_focus.set(ModesFocus::EditGoal),
            ModesHit::GoalText => self.editor_focus.set(GoalEditorFocus::Text),
            ModesHit::SaveGoal => self.editor_focus.set(GoalEditorFocus::Save),
            ModesHit::CancelGoal => self.editor_focus.set(GoalEditorFocus::Cancel),
        }
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ModesHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, modes: &WorkModes, editable: bool) {
        if !self.open {
            return;
        }
        let editing = self.editing_goal;
        let overview_focused = self.overview_focus.current().copied();
        let editor_focused = self.editor_focus.current().copied();
        let animation_frame = self.animation_frame;
        let goal_buffer = self.goal_buffer.clone();
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(if editing {
            text(Text::EditPersistentGoal)
        } else {
            text(Text::WorkModes)
        })
        .width_percent(76)
        .height_percent(72)
        .min_size(66, 28)
        .max_size(132, 48)
        .border_color(Color::Magenta)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            if editing {
                draw_goal_editor(frame, area, &goal_buffer, editor_focused, editable, clicks);
            } else {
                draw_modes_overview(
                    frame,
                    area,
                    modes,
                    overview_focused,
                    animation_frame,
                    editable,
                    clicks,
                );
            }
        });
        popup.render(frame);
    }
}

impl Default for ModesUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_modes_overview(
    frame: &mut Frame<'_>,
    area: Rect,
    modes: &WorkModes,
    focused: Option<ModesFocus>,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<ModesHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["·", "•", "●", "•"][animation_frame % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {}: {}",
                    text(Text::ActiveTogether),
                    modes.active_summary()
                ),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::IndependentModesHelp)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );
    render_toggle(
        frame,
        rows[1],
        text(Text::PlanModeDescription),
        modes.plan,
        ModesHit::Plan,
        ControlState {
            focused: focused == Some(ModesFocus::Plan),
            enabled: editable,
        },
        clicks,
    );
    render_toggle(
        frame,
        rows[2],
        text(Text::ExploreModeDescription),
        modes.explore,
        ModesHit::Explore,
        ControlState {
            focused: focused == Some(ModesFocus::Explore),
            enabled: editable,
        },
        clicks,
    );
    render_toggle(
        frame,
        rows[3],
        text(Text::ReviewModeDescription),
        modes.review,
        ModesHit::Review,
        ControlState {
            focused: focused == Some(ModesFocus::Review),
            enabled: editable,
        },
        clicks,
    );
    render_toggle(
        frame,
        rows[4],
        text(Text::GoalModeDescription),
        modes.goal_enabled(),
        ModesHit::Goal,
        ControlState {
            focused: focused == Some(ModesFocus::Goal),
            enabled: editable,
        },
        clicks,
    );
    render_toggle(
        frame,
        rows[5],
        text(Text::DeepThinkingDescription),
        modes.deep_thinking,
        ModesHit::Deep,
        ControlState {
            focused: focused == Some(ModesFocus::Deep),
            enabled: editable,
        },
        clicks,
    );
    let goal_lines = modes.goal.as_ref().map_or_else(
        || vec![Line::from(text(Text::NoPersistentGoal))],
        |goal| {
            vec![
                Line::from(Span::styled(
                    format!(
                        "{} · {} · {} {}",
                        text(Text::Goal),
                        goal_status_label(goal.status),
                        text(Text::Revision),
                        goal.revision
                    ),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(truncate_for_display(
                    &sanitize_for_display(&goal.objective),
                    1_000,
                )),
                Line::from(format!(
                    "{}: {} {} · {} {} · {} {}",
                    text(Text::Progress),
                    goal.completed_steps.len(),
                    text(Text::CompleteLabel),
                    goal.next_steps.len(),
                    text(Text::NextLabel),
                    text(Text::CheckedTurn),
                    goal.last_checked_turn
                        .map_or_else(|| "—".to_owned(), |turn| turn.to_string())
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(goal_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::PersistentObjective))),
            )
            .wrap(Wrap { trim: false }),
        rows[6],
    );
    let buttons = Layout::horizontal([
        Constraint::Length(16),
        Constraint::Fill(1),
        Constraint::Length(22),
    ])
    .split(rows[7]);
    render_button(
        frame,
        buttons[0],
        text(Text::CloseEsc),
        ModesHit::Close,
        ButtonStyle::default(),
        ControlState {
            focused: focused == Some(ModesFocus::Close),
            enabled: true,
        },
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::EditGoal),
        ModesHit::EditGoal,
        ButtonStyle::primary(),
        ControlState {
            focused: focused == Some(ModesFocus::EditGoal),
            enabled: editable,
        },
        clicks,
    );
}

fn draw_goal_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    goal_buffer: &str,
    focused: Option<GoalEditorFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<ModesHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(text(Text::GoalEditorHelp)), rows[0]);
    let safe = truncate_for_display(&sanitize_for_display(goal_buffer), 16_000);
    let editor = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(GoalEditorFocus::Text) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(
            " {} · {} {} ",
            text(Text::Objective),
            goal_buffer.len(),
            text(Text::Bytes)
        ));
    frame.render_widget(
        Paragraph::new(safe)
            .block(editor)
            .wrap(Wrap { trim: false }),
        rows[1],
    );
    clicks.register(rows[1], ModesHit::GoalText);
    let buttons = Layout::horizontal([
        Constraint::Length(18),
        Constraint::Length(1),
        Constraint::Length(18),
        Constraint::Fill(1),
    ])
    .split(rows[2]);
    render_button(
        frame,
        buttons[0],
        text(Text::SaveGoal),
        ModesHit::SaveGoal,
        ButtonStyle::primary(),
        ControlState {
            focused: focused == Some(GoalEditorFocus::Save),
            enabled: editable && has_visible_text(goal_buffer),
        },
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::Cancel),
        ModesHit::CancelGoal,
        ButtonStyle::default(),
        ControlState {
            focused: focused == Some(GoalEditorFocus::Cancel),
            enabled: true,
        },
        clicks,
    );
}

fn render_toggle(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    hit: ModesHit,
    control: ControlState,
    clicks: &mut ClickRegionRegistry<ModesHit>,
) {
    let mut state = CheckBoxState::new(checked);
    state.set_focused(control.focused);
    state.set_enabled(control.enabled);
    let region = CheckBox::new(label, &state)
        .style(
            CheckBoxStyle::custom("[ON]", "[OFF]")
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    if control.enabled {
        clicks.register(region.area, hit);
    }
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: ModesHit,
    style: ButtonStyle,
    control: ControlState,
    clicks: &mut ClickRegionRegistry<ModesHit>,
) {
    let mut state = if control.enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(control.focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style)
        .render_stateful(area, frame.buffer_mut());
    if control.enabled {
        clicks.register(region.area, hit);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanFocus {
    Approve,
    Edit,
    Reject,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlanHit {
    Approve,
    Edit,
    Reject,
    Text,
}

#[derive(Debug, Clone)]
pub struct PlanApprovalUiState {
    review_identity: Option<(TurnId, u64)>,
    dialog: DialogState<()>,
    focus: FocusManager<PlanFocus>,
    clicks: ClickRegionRegistry<PlanHit>,
    plan_buffer: String,
    editing: bool,
}

impl PlanApprovalUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            PlanFocus::Approve,
            PlanFocus::Edit,
            PlanFocus::Reject,
            PlanFocus::Text,
        ] {
            focus.register(item);
        }
        focus.set(PlanFocus::Approve);
        Self {
            review_identity: None,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            plan_buffer: String::new(),
            editing: false,
        }
    }

    pub fn sync(&mut self, review: Option<&Arc<PlanReview>>) {
        match review {
            Some(review) if self.review_identity != Some((review.turn_id, review.review_id)) => {
                self.review_identity = Some((review.turn_id, review.review_id));
                self.plan_buffer.clone_from(&review.plan);
                self.editing = false;
                self.focus.set(PlanFocus::Approve);
                self.dialog.show();
            }
            None => {
                self.review_identity = None;
                self.plan_buffer.clear();
                self.editing = false;
                self.dialog.hide();
                self.clicks.clear();
            }
            Some(_) => {}
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.review_identity.is_some()
    }

    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.editing
    }

    pub fn set_editing(&mut self, editing: bool) {
        self.editing = editing;
        self.focus.set(if editing {
            PlanFocus::Text
        } else {
            PlanFocus::Approve
        });
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    #[must_use]
    pub fn focused(&self) -> Option<PlanFocus> {
        self.focus.current().copied()
    }

    pub fn focus(&mut self, focus: PlanFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn plan(&self) -> &str {
        &self.plan_buffer
    }

    #[must_use]
    pub fn plan_has_visible_text(&self) -> bool {
        has_visible_text(&self.plan_buffer)
    }

    pub fn push_char(&mut self, character: char) {
        if self.editing
            && !character.is_control()
            && self.plan_buffer.len().saturating_add(character.len_utf8())
                <= crate::agent::modes::MAX_PLAN_BYTES
        {
            self.plan_buffer.push(character);
        }
    }

    pub fn push_text(&mut self, text: &str) {
        if !self.editing {
            return;
        }
        for character in text.chars() {
            if self.plan_buffer.len().saturating_add(character.len_utf8())
                > crate::agent::modes::MAX_PLAN_BYTES
            {
                break;
            }
            if character == '\n' || character == '\t' || !character.is_control() {
                self.plan_buffer.push(character);
            }
        }
    }

    pub fn pop_char(&mut self) {
        if self.editing
            && let Some((index, _)) = self.plan_buffer.grapheme_indices(true).next_back()
        {
            self.plan_buffer.truncate(index);
        }
    }

    pub fn newline(&mut self) {
        if self.editing && self.plan_buffer.len() < crate::agent::modes::MAX_PLAN_BYTES {
            self.plan_buffer.push('\n');
        }
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PlanHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, review: &PlanReview) {
        let focused = self.focus.current().copied();
        let editing = self.editing;
        let plan = self.plan_buffer.clone();
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::ReadOnlyPlanReview))
            .width_percent(88)
            .height_percent(84)
            .min_size(72, 22)
            .max_size(170, 58)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            let rows = Layout::vertical([
                Constraint::Length(3),
                Constraint::Min(10),
                Constraint::Length(3),
            ])
            .split(area);
            let mode = review
                .reasoning_mode
                .map_or_else(|| text(Text::Standard).to_owned(), |m| m.to_string());
            frame.render_widget(
                Paragraph::new(format!(
                    "{}: {} · {}: {} · {}: {}\n{}",
                    text(Text::Model),
                    sanitize_for_display(&review.deployment),
                    text(Text::Effort),
                    review.reasoning_effort,
                    text(Text::Mode),
                    mode,
                    text(Text::PlanReviewHelp)
                )),
                rows[0],
            );
            let safe = truncate_for_display(&sanitize_for_display(&plan), 48_000);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(if editing && focused == Some(PlanFocus::Text) {
                    Style::default().fg(Color::LightCyan)
                } else {
                    Style::default().fg(Color::Gray)
                })
                .title(format!(
                    " {} ",
                    if editing {
                        text(Text::PlanEditingTitle)
                    } else {
                        text(Text::PlanPreviewTitle)
                    }
                ));
            frame.render_widget(
                Paragraph::new(safe).block(block).wrap(Wrap { trim: false }),
                rows[1],
            );
            clicks.register(rows[1], PlanHit::Text);
            let buttons = Layout::horizontal([
                Constraint::Length(22),
                Constraint::Length(1),
                Constraint::Length(18),
                Constraint::Fill(1),
                Constraint::Length(18),
            ])
            .split(rows[2]);
            render_plan_button(
                frame,
                buttons[0],
                text(Text::ApproveExecute),
                PlanHit::Approve,
                ButtonStyle::primary(),
                ControlState {
                    focused: focused == Some(PlanFocus::Approve),
                    enabled: has_visible_text(&plan),
                },
                clicks,
            );
            render_plan_button(
                frame,
                buttons[2],
                if editing {
                    text(Text::FinishEdit)
                } else {
                    text(Text::EditPlan)
                },
                PlanHit::Edit,
                ButtonStyle::default(),
                ControlState {
                    focused: focused == Some(PlanFocus::Edit),
                    enabled: true,
                },
                clicks,
            );
            render_plan_button(
                frame,
                buttons[4],
                text(Text::Reject),
                PlanHit::Reject,
                ButtonStyle::danger(),
                ControlState {
                    focused: focused == Some(PlanFocus::Reject),
                    enabled: true,
                },
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for PlanApprovalUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn render_plan_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: PlanHit,
    style: ButtonStyle,
    control: ControlState,
    clicks: &mut ClickRegionRegistry<PlanHit>,
) {
    let mut state = if control.enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(control.focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style)
        .render_stateful(area, frame.buffer_mut());
    if control.enabled {
        clicks.register(region.area, hit);
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    #[test]
    fn every_mode_toggle_has_a_real_mouse_region() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = ModesUiState::new();
        ui.open(&WorkModes::default());
        let mut terminal = Terminal::new(TestBackend::new(110, 34))?;
        terminal.draw(|frame| ui.draw(frame, &WorkModes::default(), true))?;
        for expected in [
            ModesHit::Plan,
            ModesHit::Explore,
            ModesHit::Review,
            ModesHit::Goal,
            ModesHit::Deep,
        ] {
            let found =
                (0..34).any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(expected)));
            if !found {
                return Err(io::Error::other(format!("missing {expected:?} hit region")).into());
            }
        }
        Ok(())
    }

    #[test]
    fn combined_modes_are_rendered_as_one_explicit_active_set()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut modes = WorkModes {
            plan: true,
            explore: true,
            review: true,
            deep_thinking: true,
            goal: None,
        };
        modes.set_goal(Some("Ship safely".to_owned()))?;
        let mut ui = ModesUiState::new();
        ui.open(&modes);
        let mut terminal = Terminal::new(TestBackend::new(110, 34))?;
        terminal.draw(|frame| ui.draw(frame, &modes, true))?;
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Active together: Plan + Explore + Review + Goal + Deep"));
        Ok(())
    }

    #[test]
    fn goal_editor_respects_byte_limit_and_removes_whole_graphemes() {
        let mut ui = ModesUiState::new();
        ui.edit_goal(&WorkModes::default());
        ui.push_goal_text(&"a".repeat(crate::agent::modes::MAX_GOAL_BYTES - 1));
        ui.push_goal_char('é');
        assert_eq!(
            ui.goal_buffer().len(),
            crate::agent::modes::MAX_GOAL_BYTES - 1
        );

        let mut ui = ModesUiState::new();
        ui.edit_goal(&WorkModes::default());
        ui.push_goal_text("e\u{301}");
        ui.pop_goal_char();
        assert!(ui.goal_buffer().is_empty());
    }

    #[test]
    fn plan_editor_respects_byte_limit_and_removes_whole_graphemes() {
        let review = Arc::new(PlanReview {
            turn_id: 1,
            review_id: 1,
            plan: String::new(),
            deployment: "test".to_owned(),
            reasoning_effort: crate::api::ReasoningEffort::Low,
            reasoning_mode: None,
        });
        let mut ui = PlanApprovalUiState::new();
        ui.sync(Some(&review));
        ui.set_editing(true);
        ui.push_text(&"a".repeat(crate::agent::modes::MAX_PLAN_BYTES - 1));
        ui.push_char('é');
        assert_eq!(ui.plan().len(), crate::agent::modes::MAX_PLAN_BYTES - 1);

        ui.sync(None);
        ui.sync(Some(&review));
        ui.set_editing(true);
        ui.push_text("e\u{301}");
        ui.pop_char();
        assert!(ui.plan().is_empty());
    }

    #[test]
    fn plan_editor_resets_when_turn_changes_even_if_review_id_repeats() {
        let first = Arc::new(PlanReview {
            turn_id: 1,
            review_id: 7,
            plan: "first plan".to_owned(),
            deployment: "test".to_owned(),
            reasoning_effort: crate::api::ReasoningEffort::Low,
            reasoning_mode: None,
        });
        let replacement = Arc::new(PlanReview {
            turn_id: 2,
            review_id: 7,
            plan: "replacement plan".to_owned(),
            deployment: "test".to_owned(),
            reasoning_effort: crate::api::ReasoningEffort::Low,
            reasoning_mode: None,
        });
        let mut ui = PlanApprovalUiState::new();
        ui.sync(Some(&first));
        ui.set_editing(true);
        ui.push_text(" stale edit");

        ui.sync(Some(&replacement));

        assert_eq!(ui.plan(), "replacement plan");
        assert!(!ui.is_editing());
    }
}
