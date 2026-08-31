use std::time::{Duration, Instant};

use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, CheckBox, CheckBoxState, CheckBoxStyle,
        DialogConfig, DialogState, PopupDialog,
    },
    state::FocusManager,
};

use super::i18n::{Text, text};

use crate::agent::AutoApprovalPolicy;

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApprovalFocus {
    All,
    Plans,
    Workspace,
    Shell,
    McpRead,
    McpMutating,
    Continuations,
    SubagentShell,
    SubagentChanges,
    Close,
}

pub type ApprovalHit = ApprovalFocus;

#[derive(Debug, Clone)]
pub struct ApprovalCenterUiState {
    open: bool,
    dialog: DialogState<()>,
    focus: FocusManager<ApprovalFocus>,
    clicks: ClickRegionRegistry<ApprovalHit>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl ApprovalCenterUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for target in [
            ApprovalFocus::All,
            ApprovalFocus::Plans,
            ApprovalFocus::Workspace,
            ApprovalFocus::Shell,
            ApprovalFocus::McpRead,
            ApprovalFocus::McpMutating,
            ApprovalFocus::Continuations,
            ApprovalFocus::SubagentShell,
            ApprovalFocus::SubagentChanges,
            ApprovalFocus::Close,
        ] {
            focus.register(target);
        }
        focus.set(ApprovalFocus::All);
        Self {
            open: false,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.focus.set(ApprovalFocus::All);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
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

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    #[must_use]
    pub fn focused(&self) -> Option<ApprovalFocus> {
        self.focus.current().copied()
    }

    pub fn focus(&mut self, target: ApprovalFocus) {
        self.focus.set(target);
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ApprovalHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, policy: AutoApprovalPolicy) {
        if !self.open {
            return;
        }
        let focused = self.focused();
        let animation = self.animation_frame;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::AutoApprovalCenter))
            .width_percent(74)
            .height_percent(80)
            .min_size(72, 28)
            .max_size(112, 40)
            .border_color(Color::Yellow)
            .focused_border_color(Color::LightYellow)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(frame, area, policy, focused, animation, clicks);
        });
        popup.render(frame);
    }
}

impl Default for ApprovalCenterUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    policy: AutoApprovalPolicy,
    focused: Option<ApprovalFocus>,
    animation: usize,
    clicks: &mut ClickRegionRegistry<ApprovalHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Min(16),
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["◐", "◓", "◑", "◒"][animation % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {}/8 {}",
                    policy.enabled_count(),
                    text(Text::AutomaticClassesEnabled)
                ),
                Style::default()
                    .fg(Color::LightYellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::SessionScopedApprovalHelp)),
            Line::from(Span::styled(
                text(Text::ApprovalPrecedenceHelp),
                Style::default().fg(Color::LightRed),
            )),
        ]),
        rows[0],
    );

    draw_check(
        frame,
        rows[1],
        text(Text::AutoApproveAllSafe),
        policy.all_enabled(),
        ApprovalFocus::All,
        focused,
        clicks,
    );

    let choices = Layout::vertical([Constraint::Length(2); 8]).split(rows[2]);
    for (row, label, checked, target) in [
        (
            choices[0],
            text(Text::ApprovalPlans),
            policy.plans,
            ApprovalFocus::Plans,
        ),
        (
            choices[1],
            text(Text::ApprovalWorkspace),
            policy.workspace_changes,
            ApprovalFocus::Workspace,
        ),
        (
            choices[2],
            text(Text::ApprovalMainShell),
            policy.shell,
            ApprovalFocus::Shell,
        ),
        (
            choices[3],
            text(Text::ApprovalMcpRead),
            policy.mcp_read_only,
            ApprovalFocus::McpRead,
        ),
        (
            choices[4],
            text(Text::ApprovalMcpMutating),
            policy.mcp_mutating,
            ApprovalFocus::McpMutating,
        ),
        (
            choices[5],
            text(Text::ApprovalContinuation),
            policy.continuations,
            ApprovalFocus::Continuations,
        ),
        (
            choices[6],
            text(Text::ApprovalSubagentShell),
            policy.subagent_shell,
            ApprovalFocus::SubagentShell,
        ),
        (
            choices[7],
            text(Text::ApprovalSubagentChanges),
            policy.subagent_changes,
            ApprovalFocus::SubagentChanges,
        ),
    ] {
        draw_check(frame, row, label, checked, target, focused, clicks);
    }

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                text(Text::ApprovalRiskNote),
                Style::default().fg(Color::Yellow),
            )),
            Line::from(text(Text::ApprovalImmediateHelp)),
        ]),
        rows[3],
    );
    render_close(frame, rows[4], focused, clicks);
}

fn draw_check(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    target: ApprovalFocus,
    focused: Option<ApprovalFocus>,
    clicks: &mut ClickRegionRegistry<ApprovalHit>,
) {
    let mut state = CheckBoxState::new(checked);
    state.set_focused(focused == Some(target));
    let region = CheckBox::new(label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::AutoMarker), text(Text::AskMarker))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, target);
}

fn render_close(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<ApprovalFocus>,
    clicks: &mut ClickRegionRegistry<ApprovalHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(20),
        Constraint::Fill(1),
    ])
    .split(area);
    let mut state = ButtonState::enabled();
    state.set_focused(focused == Some(ApprovalFocus::Close));
    let region = Button::new(text(Text::DoneEsc), &state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::primary())
        .render_stateful(columns[1], frame.buffer_mut());
    clicks.register(region.area, ApprovalFocus::Close);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{ApprovalCenterUiState, ApprovalFocus};
    use crate::agent::AutoApprovalPolicy;

    #[test]
    fn every_approval_class_has_mouse_and_tab_access() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = ApprovalCenterUiState::new();
        ui.open();
        let backend = TestBackend::new(110, 42);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| ui.draw(frame, AutoApprovalPolicy::default()))?;
        for target in [
            ApprovalFocus::All,
            ApprovalFocus::Plans,
            ApprovalFocus::Workspace,
            ApprovalFocus::Shell,
            ApprovalFocus::McpRead,
            ApprovalFocus::McpMutating,
            ApprovalFocus::Continuations,
            ApprovalFocus::SubagentShell,
            ApprovalFocus::SubagentChanges,
            ApprovalFocus::Close,
        ] {
            assert!(
                (0..42).any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(target)))
            );
        }
        assert_eq!(ui.focused(), Some(ApprovalFocus::All));
        ui.next_focus();
        assert_eq!(ui.focused(), Some(ApprovalFocus::Plans));
        for _ in 0..9 {
            ui.next_focus();
        }
        assert_eq!(ui.focused(), Some(ApprovalFocus::All));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(ApprovalFocus::Close));
        Ok(())
    }
}
