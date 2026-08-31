use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, PopupDialog,
    },
    state::FocusManager,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::{
    app::{PendingConfirmation, PendingContinuation, PendingMcpConfirmation},
    i18n::{Text, text},
    render::sanitize_for_display,
    syntax,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfirmationChoice {
    Approve,
    TrustExactForSession,
    Decline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContinuationChoice {
    Stop,
    Continue,
}

#[derive(Debug, Clone)]
pub struct ContinuationUiState {
    dialog: DialogState<()>,
    focus: FocusManager<ContinuationChoice>,
    click_regions: ClickRegionRegistry<ContinuationChoice>,
}

impl ContinuationUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(ContinuationChoice::Stop);
        focus.register(ContinuationChoice::Continue);
        focus.set(ContinuationChoice::Continue);
        Self {
            dialog: DialogState::new(()),
            focus,
            click_regions: ClickRegionRegistry::new(),
        }
    }

    pub fn reset(&mut self) {
        self.dialog.hide();
        self.focus.set(ContinuationChoice::Continue);
        self.click_regions.clear();
    }

    pub fn next(&mut self) {
        self.focus.next();
    }

    pub fn previous(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, choice: ContinuationChoice) {
        self.focus.set(choice);
    }

    #[must_use]
    pub fn selected(&self) -> Option<ContinuationChoice> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ContinuationChoice> {
        self.click_regions.handle_click(column, row).copied()
    }
}

impl Default for ContinuationUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct ConfirmationUiState {
    dialog: DialogState<()>,
    focus: FocusManager<ConfirmationChoice>,
    click_regions: ClickRegionRegistry<ConfirmationChoice>,
    command_area: Option<Rect>,
    session_trust_available: bool,
}

impl ConfirmationUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(ConfirmationChoice::Decline);
        focus.register(ConfirmationChoice::Approve);
        focus.register(ConfirmationChoice::TrustExactForSession);
        focus.set(ConfirmationChoice::Approve);
        Self {
            dialog: DialogState::new(()),
            focus,
            click_regions: ClickRegionRegistry::new(),
            command_area: None,
            session_trust_available: false,
        }
    }

    pub fn reset(&mut self) {
        self.dialog.hide();
        self.focus.set(ConfirmationChoice::Approve);
        self.click_regions.clear();
        self.command_area = None;
        self.session_trust_available = false;
    }

    pub fn next(&mut self) {
        self.focus.next();
        self.skip_unavailable_trust(true);
    }

    pub fn previous(&mut self) {
        self.focus.prev();
        self.skip_unavailable_trust(false);
    }

    pub fn focus(&mut self, choice: ConfirmationChoice) {
        if choice != ConfirmationChoice::TrustExactForSession || self.session_trust_available {
            self.focus.set(choice);
        }
    }

    pub fn set_session_trust_available(&mut self, available: bool) {
        self.session_trust_available = available;
        if !available && self.selected() == Some(ConfirmationChoice::TrustExactForSession) {
            self.focus.set(ConfirmationChoice::Approve);
        }
    }

    fn skip_unavailable_trust(&mut self, forward: bool) {
        if !self.session_trust_available
            && self.selected() == Some(ConfirmationChoice::TrustExactForSession)
        {
            if forward {
                self.focus.next();
            } else {
                self.focus.prev();
            }
        }
    }

    #[must_use]
    pub fn selected(&self) -> Option<ConfirmationChoice> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ConfirmationChoice> {
        self.click_regions.handle_click(column, row).copied()
    }

    #[must_use]
    pub fn command_contains(&self, column: u16, row: u16) -> bool {
        self.command_area.is_some_and(|area| {
            column >= area.x
                && column < area.x.saturating_add(area.width)
                && row >= area.y
                && row < area.y.saturating_add(area.height)
        })
    }
}

impl Default for ConfirmationUiState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn draw_confirmation_dialog(frame: &mut Frame<'_>, pending: &PendingConfirmation) {
    let mut ui = ConfirmationUiState::new();
    let _ = draw_confirmation_dialog_scrolled(frame, pending, 0, false, &mut ui);
}

pub fn draw_confirmation_dialog_scrolled(
    frame: &mut Frame<'_>,
    pending: &PendingConfirmation,
    scroll: usize,
    suffix_viewed: bool,
    ui: &mut ConfirmationUiState,
) -> Option<usize> {
    if !ui.dialog.is_visible() {
        ui.dialog.show();
    }
    ui.click_regions.clear();
    ui.command_area = None;
    ui.set_session_trust_available(pending.session_trust_available);

    let approve_focused = ui.focus.is_focused(&ConfirmationChoice::Approve);
    let trust_focused = ui
        .focus
        .is_focused(&ConfirmationChoice::TrustExactForSession);
    let decline_focused = ui.focus.is_focused(&ConfirmationChoice::Decline);
    let config = DialogConfig::new(text(Text::ConfirmShellCommand))
        .width_percent(82)
        .height_percent(78)
        .min_size(60, 16)
        .max_size(180, 60)
        .border_color(Color::Yellow)
        .focused_border_color(Color::Yellow)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
    let mut max_scroll = None;
    let dialog = &mut ui.dialog;
    let click_regions = &mut ui.click_regions;
    let command_area = &mut ui.command_area;
    let mut popup = PopupDialog::new(&config, dialog, |frame, area, _content| {
        max_scroll = draw_confirmation_content(
            frame,
            area,
            pending,
            scroll,
            suffix_viewed,
            approve_focused,
            trust_focused,
            decline_focused,
            click_regions,
            command_area,
        );
    });
    popup.render(frame);
    max_scroll
}

pub fn draw_mcp_confirmation_dialog_scrolled(
    frame: &mut Frame<'_>,
    pending: &PendingMcpConfirmation,
    scroll: usize,
    suffix_viewed: bool,
    ui: &mut ConfirmationUiState,
) -> Option<usize> {
    if !ui.dialog.is_visible() {
        ui.dialog.show();
    }
    ui.click_regions.clear();
    ui.command_area = None;
    ui.set_session_trust_available(false);

    let approve_focused = ui.focus.is_focused(&ConfirmationChoice::Approve);
    let decline_focused = ui.focus.is_focused(&ConfirmationChoice::Decline);
    let config = DialogConfig::new(text(Text::ApproveMcpTool))
        .width_percent(82)
        .height_percent(78)
        .min_size(60, 17)
        .max_size(180, 60)
        .border_color(Color::LightMagenta)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
    let mut max_scroll = None;
    let dialog = &mut ui.dialog;
    let click_regions = &mut ui.click_regions;
    let command_area = &mut ui.command_area;
    let mut popup = PopupDialog::new(&config, dialog, |frame, area, _content| {
        max_scroll = draw_mcp_confirmation_content(
            frame,
            area,
            pending,
            scroll,
            suffix_viewed,
            approve_focused,
            decline_focused,
            click_regions,
            command_area,
        );
    });
    popup.render(frame);
    max_scroll
}

#[allow(clippy::too_many_arguments)]
fn draw_mcp_confirmation_content(
    frame: &mut Frame<'_>,
    area: Rect,
    pending: &PendingMcpConfirmation,
    scroll: usize,
    suffix_viewed: bool,
    approve_focused: bool,
    decline_focused: bool,
    click_regions: &mut ClickRegionRegistry<ConfirmationChoice>,
    command_area: &mut Option<Rect>,
) -> Option<usize> {
    if area.width < 8 || area.height < 6 {
        frame.render_widget(Paragraph::new(text(Text::McpApprovalResize)), area);
        return None;
    }
    let chunks = Layout::vertical([
        Constraint::Length(7),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(area);
    let header = vec![
        Line::from(Span::styled(
            format!(
                "{} #{} | {} #{} | {}::{}",
                text(Text::Turn),
                pending.turn_id,
                text(Text::ActionLabel),
                pending.action_id,
                sanitize_for_display(&pending.call.server),
                sanitize_for_display(&pending.call.tool),
            ),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::FunctionLabel),
            sanitize_for_display(&pending.call.function_name)
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::CallIdLabel),
            sanitize_for_display(&pending.call.call_id)
        )),
        Line::from(Span::styled(
            text(Text::ExactMcpApprovalBinding),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(Span::styled(
            text(Text::McpExternalCodeWarning),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(text(Text::SessionGrantHelp)),
        Line::from(format!(
            "{}: {}",
            text(Text::PolicyLabel),
            sanitize_for_display(&pending.reason)
        )),
    ];
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), chunks[0]);

    let argument_block = Block::default().borders(Borders::ALL).title(format!(
        " {} | Up/Down/Page/Home/End ",
        text(Text::SanitizedMcpRequest)
    ));
    let argument_inner = argument_block.inner(chunks[1]);
    if argument_inner.width == 0 || argument_inner.height == 0 {
        return None;
    }
    *command_area = Some(argument_inner);
    let raw_arguments = serde_json::to_string_pretty(&pending.call.arguments)
        .unwrap_or_else(|error| format!("{{\"preview_error\":\"{error}\"}}"));
    // Security invariant: terminal controls and bidi overrides are neutralized
    // before untrusted MCP text reaches the syntax highlighter.
    let safe_arguments = sanitize_for_display(&raw_arguments);
    let wrapped = wrap_escaped_command(&safe_arguments, usize::from(argument_inner.width));
    let highlighted = syntax::highlight_source("arguments.json", &wrapped.join("\n"));
    let visible_rows = usize::from(argument_inner.height);
    let max_scroll = wrapped.len().saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);
    let visible_lines = wrapped
        .into_iter()
        .enumerate()
        .skip(scroll)
        .take(visible_rows)
        .map(|(index, line)| {
            highlighted
                .as_ref()
                .and_then(|rows| rows.get(index))
                .map_or_else(|| Line::from(line), |spans| Line::from(spans.clone()))
        })
        .collect::<Vec<_>>();
    frame.render_widget(argument_block, chunks[1]);
    frame.render_widget(Paragraph::new(visible_lines), argument_inner);

    let review = if max_scroll > 0 && !suffix_viewed {
        Span::styled(text(Text::ReviewToEnd), Style::default().fg(Color::Yellow))
    } else {
        Span::styled(
            format!(
                "{} · {}",
                text(Text::SanitizedMcpRequest),
                text(Text::ReviewedLabel)
            ),
            Style::default().fg(Color::Green),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(review)), chunks[2]);

    let approve_enabled = max_scroll == 0 || suffix_viewed;
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(22),
        Constraint::Fill(1),
    ])
    .split(chunks[3]);
    let mut decline_state = ButtonState::enabled();
    decline_state.set_focused(decline_focused);
    let decline = Button::new(text(Text::DeclineEsc), &decline_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::danger());
    let decline_region = decline.render_stateful(columns[1], frame.buffer_mut());
    click_regions.register(decline_region.area, ConfirmationChoice::Decline);

    let mut approve_state = if approve_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    approve_state.set_focused(approve_focused);
    let approve = Button::new(text(Text::ExecuteEnter), &approve_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::success());
    let approve_region = approve.render_stateful(columns[3], frame.buffer_mut());
    click_regions.register(approve_region.area, ConfirmationChoice::Approve);
    Some(max_scroll)
}

#[allow(clippy::too_many_arguments)]
fn draw_confirmation_content(
    frame: &mut Frame<'_>,
    area: Rect,
    pending: &PendingConfirmation,
    scroll: usize,
    suffix_viewed: bool,
    approve_focused: bool,
    trust_focused: bool,
    decline_focused: bool,
    click_regions: &mut ClickRegionRegistry<ConfirmationChoice>,
    command_area: &mut Option<Rect>,
) -> Option<usize> {
    if area.width < 8 || area.height < 5 {
        frame.render_widget(Paragraph::new(text(Text::ConfirmationResize)), area);
        return None;
    }

    let chunks = Layout::vertical([
        Constraint::Length(8),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(area);
    let model_flag = if pending.model_requested {
        text(Text::YesLabel)
    } else {
        text(Text::NoLabel)
    };
    let digest = hex_digest(pending.command_digest.as_bytes());
    let header = vec![
        Line::from(Span::styled(
            format!(
                "{} #{} | {} #{} | {}",
                text(Text::Turn),
                pending.turn_id,
                text(Text::ActionLabel),
                pending.action_id,
                sanitize_for_display(pending.action.tool_name())
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{} · {}: {model_flag}",
            text(Text::Model),
            text(Text::ApprovalModeLabel)
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::ReasonLabel),
            sanitize_for_display(&pending.reason.to_string())
        )),
        Line::from(Span::styled(
            text(Text::UserPermissionsWarning),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            text(Text::ShellCommandScopeWarning),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(format!("{}: {}", text(Text::Bytes), pending.command_bytes)),
        Line::from(format!("{}: {digest}", text(Text::Sha256Label))),
        Line::from(if pending.session_trust_available {
            text(Text::SessionGrantHelp)
        } else {
            text(Text::ForcedConfirmationHelp)
        }),
    ];
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), chunks[0]);

    let command_block = Block::default().borders(Borders::ALL).title(format!(
        " {} | Up/Down/PageUp/PageDown/Home/End ",
        text(Text::SanitizedCommand)
    ));
    let command_inner = command_block.inner(chunks[1]);
    if command_inner.width == 0 || command_inner.height == 0 {
        return None;
    }
    *command_area = Some(command_inner);
    let command_lines = wrap_escaped_command(&pending.command, usize::from(command_inner.width));
    let visible_rows = usize::from(command_inner.height);
    let max_scroll = command_lines.len().saturating_sub(visible_rows);
    let scroll = scroll.min(max_scroll);
    let visible_lines = command_lines
        .into_iter()
        .skip(scroll)
        .take(visible_rows)
        .map(Line::from)
        .collect::<Vec<_>>();
    frame.render_widget(command_block, chunks[1]);
    // Rows are hard-wrapped above and sliced explicitly. Ratatui's word-wrap
    // algorithm can add rows that are absent from a display-width estimate,
    // which could otherwise make a command suffix invisible after `End`.
    frame.render_widget(Paragraph::new(visible_lines), command_inner);

    let review = if max_scroll > 0 && !suffix_viewed {
        Span::styled(text(Text::ReviewToEnd), Style::default().fg(Color::Yellow))
    } else {
        Span::styled(
            format!(
                "{} · {}",
                text(Text::SanitizedCommand),
                text(Text::ReviewedLabel)
            ),
            Style::default().fg(Color::Green),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(review)), chunks[2]);

    let approve_enabled = max_scroll == 0 || suffix_viewed;
    let button_areas = if chunks[3].width >= 78 {
        let columns = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(20),
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Length(2),
            Constraint::Length(30),
            Constraint::Fill(1),
        ])
        .split(chunks[3]);
        [columns[1], columns[3], columns[5]]
    } else {
        let columns = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Percentage(30),
            Constraint::Percentage(40),
        ])
        .split(chunks[3]);
        [columns[0], columns[1], columns[2]]
    };

    let mut decline_state = ButtonState::enabled();
    decline_state.set_focused(decline_focused);
    let decline = Button::new(text(Text::DeclineEsc), &decline_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::danger());
    let decline_region = decline.render_stateful(button_areas[0], frame.buffer_mut());
    click_regions.register(decline_region.area, ConfirmationChoice::Decline);

    let mut approve_state = if approve_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    approve_state.set_focused(approve_focused);
    let approve = Button::new(text(Text::OnceEnter), &approve_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::success());
    let approve_region = approve.render_stateful(button_areas[1], frame.buffer_mut());
    click_regions.register(approve_region.area, ConfirmationChoice::Approve);
    let trust_enabled = approve_enabled && pending.session_trust_available;
    let mut trust_state = if trust_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    trust_state.set_focused(trust_focused);
    let trust = Button::new(text(Text::TrustExactSession), &trust_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::primary());
    let trust_region = trust.render_stateful(button_areas[2], frame.buffer_mut());
    if trust_enabled {
        click_regions.register(trust_region.area, ConfirmationChoice::TrustExactForSession);
    }
    Some(max_scroll)
}

#[must_use]
pub fn escape_shell_command(command: &str) -> String {
    sanitize_for_display(command)
}

#[must_use]
pub fn escaped_command_rows(command: &str, width: usize) -> usize {
    wrap_escaped_command(command, width).len()
}

fn wrap_escaped_command(command: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let escaped = escape_shell_command(command);
    let mut rows = Vec::new();

    for logical_line in escaped.split('\n') {
        if logical_line.is_empty() {
            rows.push(String::new());
            continue;
        }

        let mut row = String::new();
        let mut row_width = 0_usize;
        for grapheme in logical_line.graphemes(true) {
            let grapheme_width = UnicodeWidthStr::width(grapheme);
            if row_width > 0 && row_width.saturating_add(grapheme_width) > width {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }

            row.push_str(grapheme);
            row_width = row_width.saturating_add(grapheme_width);
            if row_width >= width {
                rows.push(std::mem::take(&mut row));
                row_width = 0;
            }
        }
        if !row.is_empty() {
            rows.push(row);
        }
    }

    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

fn hex_digest(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(64);
    for byte in bytes {
        let _ = write!(value, "{byte:02x}");
    }
    value
}

pub fn draw_continuation_dialog(
    frame: &mut Frame<'_>,
    pending: PendingContinuation,
    ui: &mut ContinuationUiState,
) {
    if !ui.dialog.is_visible() {
        ui.dialog.show();
    }
    ui.click_regions.clear();
    let continue_focused = ui.focus.is_focused(&ContinuationChoice::Continue);
    let stop_focused = ui.focus.is_focused(&ContinuationChoice::Stop);
    let config = DialogConfig::new(text(Text::ToolIterationLimit))
        .width_percent(64)
        .height_percent(42)
        .min_size(52, 12)
        .max_size(100, 20)
        .border_color(Color::Magenta)
        .focused_border_color(Color::LightMagenta)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
    let dialog = &mut ui.dialog;
    let click_regions = &mut ui.click_regions;
    let mut popup = PopupDialog::new(&config, dialog, |frame, area, _content| {
        let rows = Layout::vertical([
            Constraint::Min(6),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(area);
        let lines = vec![
            Line::from(Span::styled(
                format!(
                    "{} #{} · {}",
                    text(Text::Turn),
                    pending.turn_id,
                    text(Text::Paused)
                ),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!(
                "{}: {}/{} · {}",
                text(Text::Completed),
                pending.completed_iterations,
                pending.max_iterations,
                text(Text::ToolIterationLimit)
            )),
            Line::from(text(Text::ContinueWindowHelp)),
            Line::from(text(Text::ClickOrTabHelp)),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(text(Text::InterruptResetHint)).alignment(Alignment::Center),
            rows[1],
        );

        let buttons = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(20),
            Constraint::Length(2),
            Constraint::Length(20),
            Constraint::Fill(1),
        ])
        .split(rows[2]);
        let mut stop_state = ButtonState::enabled();
        stop_state.set_focused(stop_focused);
        let stop = Button::new(text(Text::StopEsc), &stop_state)
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::danger());
        let stop_region = stop.render_stateful(buttons[1], frame.buffer_mut());
        click_regions.register(stop_region.area, ContinuationChoice::Stop);

        let mut continue_state = ButtonState::enabled();
        continue_state.set_focused(continue_focused);
        let continue_button = Button::new(text(Text::ContinueEnter), &continue_state)
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::success());
        let continue_region = continue_button.render_stateful(buttons[3], frame.buffer_mut());
        click_regions.register(continue_region.area, ContinuationChoice::Continue);
    });
    popup.render(frame);
}

#[must_use]
pub fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let percent_x = percent_x.min(100);
    let percent_y = percent_y.min(100);
    let vertical_margin = (100_u16.saturating_sub(percent_y)) / 2;
    let horizontal_margin = (100_u16.saturating_sub(percent_x)) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(vertical_margin),
            Constraint::Percentage(percent_y),
            Constraint::Percentage(vertical_margin),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(horizontal_margin),
            Constraint::Percentage(percent_x),
            Constraint::Percentage(horizontal_margin),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::{
        ContinuationChoice, ContinuationUiState, centered_rect, draw_continuation_dialog,
        escape_shell_command, wrap_escaped_command,
    };
    use crate::ui::app::PendingContinuation;
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn centered_rect_stays_inside_parent() {
        let parent = Rect::new(5, 7, 100, 40);
        let child = centered_rect(60, 50, parent);
        assert!(child.x >= parent.x);
        assert!(child.y >= parent.y);
        assert!(child.right() <= parent.right());
        assert!(child.bottom() <= parent.bottom());
    }

    #[test]
    fn shell_view_never_emits_escape_controls() {
        let escaped = escape_shell_command("echo\u{1b}[31m\u{202e}");
        assert!(!escaped.contains('\u{1b}'));
        assert!(!escaped.contains('\u{202e}'));
        assert!(escaped.contains("\\x1b"));
    }

    #[test]
    fn command_rows_use_exact_hard_wrap_instead_of_word_boundaries() {
        let rows = wrap_escaped_command("aaaaaa aaaaaa", 10);
        assert_eq!(rows, ["aaaaaa aaa", "aaa"]);
        assert!(
            rows.iter()
                .all(|row| UnicodeWidthStr::width(row.as_str()) <= 10)
        );
    }

    #[test]
    fn continuation_dialog_exposes_a_real_clickable_continue_button()
    -> Result<(), Box<dyn std::error::Error>> {
        let pending = PendingContinuation {
            turn_id: 7,
            continuation_id: 3,
            completed_iterations: 20,
            max_iterations: 20,
        };
        let mut ui = ContinuationUiState::new();
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| draw_continuation_dialog(frame, pending, &mut ui))?;

        let buffer = terminal.backend().buffer();
        let mut click = None;
        for row in 0..buffer.area.height {
            let rendered = (0..buffer.area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>();
            if let Some(column) = rendered.find("Continue (Enter)") {
                click = Some((u16::try_from(column).unwrap_or(0), row));
                break;
            }
        }
        let (column, row) = click.ok_or_else(|| std::io::Error::other("button not rendered"))?;
        assert_eq!(ui.clicked(column, row), Some(ContinuationChoice::Continue));
        Ok(())
    }
}
