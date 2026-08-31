use std::collections::BTreeMap;

use super::actions::ClickRegionRegistry;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{Button, ButtonState, ButtonStyle, ButtonVariant},
    state::FocusManager,
};

use crate::terminal::{
    TerminalColor, TerminalControl, TerminalControlError, TerminalFailure, TerminalFleetSnapshot,
    TerminalMouseEncoding, TerminalMouseMode, TerminalNotice, TerminalSessionId,
    TerminalSessionSnapshot, TerminalStatus, TerminalStyle,
};

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalFocus {
    New,
    Stop,
    Close,
    Latest,
    Session(TerminalSessionId),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TerminalHit {
    New,
    Stop,
    Close,
    Latest,
    Session(TerminalSessionId),
    Screen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalInputMode {
    Input,
    Toolbar,
}

#[derive(Debug, Clone)]
pub struct TerminalUiState {
    control: Option<TerminalControl>,
    selected_id: Option<TerminalSessionId>,
    focus: FocusManager<TerminalFocus>,
    input_mode: TerminalInputMode,
    clicks: ClickRegionRegistry<TerminalHit>,
    pending_resize: Option<(TerminalSessionId, u16, u16)>,
    last_requested_size: BTreeMap<TerminalSessionId, (u16, u16)>,
    seen_output: BTreeMap<TerminalSessionId, u64>,
    screen_area: Option<Rect>,
    animation_frame: usize,
    local_notice: Option<String>,
}

impl TerminalUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(TerminalFocus::New);
        focus.register(TerminalFocus::Stop);
        focus.register(TerminalFocus::Close);
        focus.register(TerminalFocus::Latest);
        focus.set(TerminalFocus::New);
        Self {
            control: None,
            selected_id: None,
            focus,
            input_mode: TerminalInputMode::Input,
            clicks: ClickRegionRegistry::new(),
            pending_resize: None,
            last_requested_size: BTreeMap::new(),
            seen_output: BTreeMap::new(),
            screen_area: None,
            animation_frame: 0,
            local_notice: None,
        }
    }

    pub fn attach_control(&mut self, control: TerminalControl) {
        self.control = Some(control);
    }

    pub fn sync(&mut self, fleet: &TerminalFleetSnapshot) {
        if let Some(notice) = &fleet.notice {
            self.local_notice = Some(terminal_notice_text(notice));
        }
        let selected_exists = self
            .selected_id
            .is_some_and(|id| fleet.sessions.iter().any(|session| session.id == id));
        if !selected_exists {
            self.selected_id = fleet.sessions.last().map(|session| session.id);
        }
        self.last_requested_size
            .retain(|id, _| fleet.sessions.iter().any(|session| session.id == *id));
        self.seen_output
            .retain(|id, _| fleet.sessions.iter().any(|session| session.id == *id));
        if self
            .pending_resize
            .is_some_and(|(id, _, _)| !fleet.sessions.iter().any(|session| session.id == id))
        {
            self.pending_resize = None;
        }
        let previous_focus = self.focus.current().copied();
        self.focus.clear();
        self.focus.register(TerminalFocus::New);
        self.focus.register(TerminalFocus::Stop);
        self.focus.register(TerminalFocus::Close);
        self.focus.register(TerminalFocus::Latest);
        self.focus.register_all(
            fleet
                .sessions
                .iter()
                .map(|session| TerminalFocus::Session(session.id)),
        );
        if let Some(previous_focus) = previous_focus {
            self.focus.set(previous_focus);
        }
        if self.focus.current().is_none() {
            self.focus.set(TerminalFocus::New);
        }
    }

    pub fn tick(&mut self) {
        self.animation_frame = self.animation_frame.wrapping_add(1);
    }

    #[must_use]
    pub const fn input_mode(&self) -> TerminalInputMode {
        self.input_mode
    }

    pub fn toggle_input_mode(&mut self) {
        self.input_mode = match self.input_mode {
            TerminalInputMode::Input => TerminalInputMode::Toolbar,
            TerminalInputMode::Toolbar => TerminalInputMode::Input,
        };
    }

    pub fn focus_input(&mut self) {
        self.input_mode = TerminalInputMode::Input;
    }

    pub fn next_control(&mut self) {
        self.focus.next();
    }

    pub fn previous_control(&mut self) {
        self.focus.prev();
    }

    #[must_use]
    pub fn selected_control(&self) -> Option<TerminalFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub const fn selected_id(&self) -> Option<TerminalSessionId> {
        self.selected_id
    }

    pub fn select(&mut self, id: TerminalSessionId) {
        self.selected_id = Some(id);
        self.input_mode = TerminalInputMode::Input;
    }

    #[must_use]
    pub fn active<'a>(
        &self,
        fleet: &'a TerminalFleetSnapshot,
    ) -> Option<&'a TerminalSessionSnapshot> {
        let id = self.selected_id?;
        fleet.sessions.iter().find(|session| session.id == id)
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<TerminalHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn activate_control(
        &mut self,
        focus: TerminalFocus,
        fleet: &TerminalFleetSnapshot,
    ) -> Result<String, TerminalControlError> {
        match focus {
            TerminalFocus::New => self.create(fleet),
            TerminalFocus::Stop => self.stop(),
            TerminalFocus::Close => self.close(),
            TerminalFocus::Latest => self.jump_to_latest(),
            TerminalFocus::Session(id) => {
                self.select(id);
                Ok(format!("{} {id}", text(Text::TerminalSelectedNotice)))
            }
        }
    }

    pub fn create(
        &mut self,
        fleet: &TerminalFleetSnapshot,
    ) -> Result<String, TerminalControlError> {
        let control = self.control()?;
        if !fleet.enabled {
            let message = text(Text::TerminalDisabledNotice).to_owned();
            self.local_notice = Some(message.clone());
            return Ok(message);
        }
        control.create()?;
        let message = text(Text::OpeningTerminalNotice).to_owned();
        self.local_notice = Some(message.clone());
        Ok(message)
    }

    pub fn stop(&mut self) -> Result<String, TerminalControlError> {
        let id = self.selected_id.ok_or(TerminalControlError::Closed)?;
        self.control()?.stop(id)?;
        let message = format!("{} {id}…", text(Text::StoppingTerminalNotice));
        self.local_notice = Some(message.clone());
        Ok(message)
    }

    pub fn close(&mut self) -> Result<String, TerminalControlError> {
        let id = self.selected_id.ok_or(TerminalControlError::Closed)?;
        self.control()?.close(id)?;
        let message = format!("{} {id}…", text(Text::ClosingTerminalNotice));
        self.local_notice = Some(message.clone());
        Ok(message)
    }

    pub fn jump_to_latest(&mut self) -> Result<String, TerminalControlError> {
        let id = self.selected_id.ok_or(TerminalControlError::Closed)?;
        self.control()?.jump_to_latest(id)?;
        let message = text(Text::FollowingLatestOutput).to_owned();
        self.local_notice = Some(message.clone());
        Ok(message)
    }

    pub fn scroll(&mut self, rows: i32) -> Result<(), TerminalControlError> {
        let id = self.selected_id.ok_or(TerminalControlError::Closed)?;
        self.control()?.scroll(id, rows)
    }

    pub fn send_key(
        &mut self,
        key: KeyEvent,
        fleet: &TerminalFleetSnapshot,
    ) -> Result<bool, TerminalControlError> {
        let Some(session) = self.active(fleet) else {
            return Ok(false);
        };
        let Some(bytes) = encode_key(key, session.frame.application_cursor) else {
            return Ok(false);
        };
        self.control()?.input(session.id, bytes)?;
        Ok(true)
    }

    pub fn paste(
        &mut self,
        text: &str,
        fleet: &TerminalFleetSnapshot,
    ) -> Result<bool, TerminalControlError> {
        let Some(session) = self.active(fleet) else {
            return Ok(false);
        };
        let bytes = encode_paste(text, session.frame.bracketed_paste);
        self.control()?.input(session.id, bytes)?;
        Ok(true)
    }

    pub fn forward_mouse(
        &mut self,
        mouse: MouseEvent,
        fleet: &TerminalFleetSnapshot,
    ) -> Result<bool, TerminalControlError> {
        let Some(area) = self.screen_area else {
            return Ok(false);
        };
        let Some(session) = self.active(fleet) else {
            return Ok(false);
        };
        let Some(bytes) = encode_mouse(
            mouse,
            area,
            session.frame.mouse_mode,
            session.frame.mouse_encoding,
        ) else {
            return Ok(false);
        };
        self.control()?.input(session.id, bytes)?;
        self.input_mode = TerminalInputMode::Input;
        Ok(true)
    }

    pub fn flush_pending_resize(&mut self) -> Result<(), TerminalControlError> {
        let Some((id, rows, cols)) = self.pending_resize else {
            return Ok(());
        };
        self.control()?.resize(id, rows, cols)?;
        self.pending_resize = None;
        Ok(())
    }

    pub fn draw_tab(&mut self, frame: &mut Frame<'_>, area: Rect, fleet: &TerminalFleetSnapshot) {
        self.sync(fleet);
        self.clicks.clear();
        self.screen_area = None;
        let sections = Layout::vertical([
            Constraint::Length(3),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(area);
        self.draw_toolbar(frame, sections[0], fleet);
        self.draw_session_tabs(frame, sections[1], fleet);
        self.draw_screen(frame, sections[2], fleet);
        self.draw_footer(frame, sections[3], fleet);
    }

    fn draw_toolbar(&mut self, frame: &mut Frame<'_>, area: Rect, fleet: &TerminalFleetSnapshot) {
        let active = self.active(fleet);
        let can_stop = active.is_some_and(|session| session.status.is_active());
        let can_close = active.is_some();
        let can_latest = active.is_some_and(|session| session.frame.scrollback_offset > 0);
        let can_new = fleet.enabled && fleet.sessions.len() < fleet.max_sessions;
        let columns = Layout::horizontal([
            Constraint::Length(13),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(15),
            Constraint::Min(1),
        ])
        .split(area);
        self.draw_button(
            frame,
            columns[0],
            &format!("＋ {}", text(Text::New)),
            TerminalHit::New,
            TerminalFocus::New,
            can_new,
            true,
        );
        self.draw_button(
            frame,
            columns[1],
            &format!("■ {}", text(Text::StopLabel)),
            TerminalHit::Stop,
            TerminalFocus::Stop,
            can_stop,
            false,
        );
        self.draw_button(
            frame,
            columns[2],
            &format!("× {}", text(Text::Close)),
            TerminalHit::Close,
            TerminalFocus::Close,
            can_close,
            false,
        );
        self.draw_button(
            frame,
            columns[3],
            &format!("↓ {}", text(Text::Latest)),
            TerminalHit::Latest,
            TerminalFocus::Latest,
            can_latest,
            false,
        );
        let capacity = format!(
            "{}/{} {}",
            fleet.sessions.len(),
            fleet.max_sessions,
            text(Text::SessionsLabel)
        );
        frame.render_widget(
            Paragraph::new(capacity)
                .style(Style::default().fg(Color::DarkGray))
                .block(Block::default().borders(Borders::BOTTOM)),
            columns[4],
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_button(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        label: &str,
        hit: TerminalHit,
        focus: TerminalFocus,
        enabled: bool,
        primary: bool,
    ) {
        let mut state = if enabled {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        state.set_focused(
            self.input_mode == TerminalInputMode::Toolbar && self.focus.is_focused(&focus),
        );
        let style = if primary {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        };
        let region = Button::new(label, &state)
            .variant(ButtonVariant::Block)
            .style(style)
            .render_stateful(area, frame.buffer_mut());
        if enabled {
            self.clicks.register(region.area, hit);
        }
    }

    fn draw_session_tabs(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        fleet: &TerminalFleetSnapshot,
    ) {
        frame.render_widget(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
            area,
        );
        let mut x = area.x;
        let right = area.right();
        for session in fleet.sessions.iter() {
            if x >= right {
                break;
            }
            let unread = self
                .seen_output
                .get(&session.id)
                .map_or(session.output_revision > 0, |seen| {
                    *seen < session.output_revision
                });
            let status = status_glyph(&session.status, self.animation_frame);
            let unread_marker = if unread { "•" } else { "" };
            let label = format!(
                " {status} {} {}{unread_marker} ",
                text(Text::Terminal),
                session.id,
            );
            let width = u16::try_from(label.chars().count())
                .unwrap_or(u16::MAX)
                .clamp(12, 22)
                .min(right.saturating_sub(x));
            if width == 0 {
                break;
            }
            let tab = Rect::new(x, area.y, width, 1);
            let selected = self.selected_id == Some(session.id);
            let focused = self.input_mode == TerminalInputMode::Toolbar
                && self.focus.is_focused(&TerminalFocus::Session(session.id));
            frame.render_widget(
                Paragraph::new(label).style(if focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else if selected {
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD | Modifier::REVERSED)
                } else {
                    Style::default().fg(status_color(&session.status))
                }),
                tab,
            );
            self.clicks.register(tab, TerminalHit::Session(session.id));
            x = x.saturating_add(width).saturating_add(1);
        }
    }

    fn draw_screen(&mut self, frame: &mut Frame<'_>, area: Rect, fleet: &TerminalFleetSnapshot) {
        let Some(session) = self.active(fleet) else {
            let message = if fleet.enabled {
                text(Text::NoTerminalOpen)
            } else {
                text(Text::TerminalConfigDisabled)
            };
            frame.render_widget(
                Paragraph::new(message)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(format!(" {} ", text(Text::Terminal))),
                    )
                    .style(Style::default().fg(Color::DarkGray))
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        };
        let status = status_label(&session.status);
        let safe_title = format!("{} {}", text(Text::Terminal), session.id);
        let safe_cwd = sanitize_for_display(&session.cwd.to_string_lossy());
        let title = if session.frame.scrollback_offset == 0 {
            format!(" {safe_title} · {status} · {safe_cwd} ")
        } else {
            format!(
                " {} · {status} · {} {} ",
                safe_title,
                session.frame.scrollback_offset,
                text(Text::LinesAboveLatest)
            )
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if self.input_mode == TerminalInputMode::Input {
                Style::default().fg(Color::LightCyan)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        let inner = block.inner(area);
        self.screen_area = Some(inner);
        frame.render_widget(block, area);
        self.clicks.register(inner, TerminalHit::Screen);
        let rows = session
            .frame
            .content
            .iter()
            .take(usize::from(inner.height))
            .map(|row| {
                Line::from(
                    row.spans
                        .iter()
                        .map(|span| Span::styled(span.text.clone(), ratatui_style(span.style)))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(rows), inner);

        if self.input_mode == TerminalInputMode::Input
            && session.frame.scrollback_offset == 0
            && !session.frame.hide_cursor
            && session.frame.cursor_row < inner.height
            && session.frame.cursor_col < inner.width
        {
            frame.set_cursor_position(Position::new(
                inner.x.saturating_add(session.frame.cursor_col),
                inner.y.saturating_add(session.frame.cursor_row),
            ));
        }
        if session.frame.scrollback_offset == 0 {
            self.seen_output.insert(session.id, session.output_revision);
        }
        let desired = (inner.height.max(2), inner.width.max(10));
        if self.last_requested_size.get(&session.id) != Some(&desired) {
            self.last_requested_size.insert(session.id, desired);
            self.pending_resize = Some((session.id, desired.0, desired.1));
        }
    }

    fn draw_footer(&self, frame: &mut Frame<'_>, area: Rect, fleet: &TerminalFleetSnapshot) {
        let mode = match self.input_mode {
            TerminalInputMode::Input => text(Text::InputMode),
            TerminalInputMode::Toolbar => text(Text::ToolbarMode),
        };
        let remote_notice = fleet.notice.as_ref().map(terminal_notice_text);
        let notice = remote_notice
            .as_deref()
            .or(self.local_notice.as_deref())
            .unwrap_or(text(Text::TerminalControlsHelp));
        let notice = sanitize_for_display(notice);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(" {mode} "),
                    Style::default()
                        .fg(Color::Black)
                        .bg(if self.input_mode == TerminalInputMode::Input {
                            Color::LightCyan
                        } else {
                            Color::Yellow
                        })
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(notice, Style::default().fg(Color::DarkGray)),
            ])),
            area,
        );
    }

    fn control(&self) -> Result<&TerminalControl, TerminalControlError> {
        self.control.as_ref().ok_or(TerminalControlError::Closed)
    }
}

impl Default for TerminalUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[must_use]
pub fn encode_mouse(
    mouse: MouseEvent,
    area: Rect,
    mode: TerminalMouseMode,
    encoding: TerminalMouseEncoding,
) -> Option<Vec<u8>> {
    if matches!(mode, TerminalMouseMode::None)
        || mouse.column < area.x
        || mouse.column >= area.right()
        || mouse.row < area.y
        || mouse.row >= area.bottom()
        || !mouse_event_enabled(mouse.kind, mode)
    {
        return None;
    }
    let x = mouse.column.saturating_sub(area.x).saturating_add(1);
    let y = mouse.row.saturating_sub(area.y).saturating_add(1);
    let (mut code, release) = match mouse.kind {
        MouseEventKind::Down(button) => (mouse_button_code(button), false),
        MouseEventKind::Up(button) => (mouse_button_code(button), true),
        MouseEventKind::Drag(button) => (mouse_button_code(button) + 32, false),
        MouseEventKind::Moved => (35, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if mouse.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mouse.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mouse.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    match encoding {
        TerminalMouseEncoding::Sgr => {
            Some(format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes())
        }
        TerminalMouseEncoding::Default => {
            let legacy_code = if release { 3 } else { code };
            let values = [legacy_code + 32, x + 32, y + 32];
            if values.iter().any(|value| *value > u16::from(u8::MAX)) {
                return None;
            }
            let mut bytes = b"\x1b[M".to_vec();
            for value in values {
                bytes.push(u8::try_from(value).ok()?);
            }
            Some(bytes)
        }
        TerminalMouseEncoding::Utf8 => {
            let legacy_code = if release { 3 } else { code };
            let mut bytes = b"\x1b[M".to_vec();
            for value in [legacy_code + 32, x + 32, y + 32] {
                let character = char::from_u32(u32::from(value))?;
                let mut encoded = [0_u8; 4];
                bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            Some(bytes)
        }
    }
}

const fn mouse_event_enabled(kind: MouseEventKind, mode: TerminalMouseMode) -> bool {
    match mode {
        TerminalMouseMode::None => false,
        TerminalMouseMode::Press => matches!(
            kind,
            MouseEventKind::Down(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        ),
        TerminalMouseMode::PressRelease => {
            !matches!(kind, MouseEventKind::Drag(_) | MouseEventKind::Moved)
        }
        TerminalMouseMode::ButtonMotion => !matches!(kind, MouseEventKind::Moved),
        TerminalMouseMode::AnyMotion => true,
    }
}

const fn mouse_button_code(button: MouseButton) -> u16 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn status_glyph(status: &TerminalStatus, animation_frame: usize) -> &'static str {
    match status {
        TerminalStatus::Starting | TerminalStatus::Stopping => {
            ["◐", "◓", "◑", "◒"][animation_frame % 4]
        }
        TerminalStatus::Running => "●",
        TerminalStatus::Exited { code: 0, .. } => "✓",
        TerminalStatus::Exited { .. } => "!",
        TerminalStatus::Failed { .. } => "×",
    }
}

const fn status_color(status: &TerminalStatus) -> Color {
    match status {
        TerminalStatus::Starting | TerminalStatus::Stopping => Color::Yellow,
        TerminalStatus::Running => Color::Green,
        TerminalStatus::Exited { code: 0, .. } => Color::DarkGray,
        TerminalStatus::Exited { .. } | TerminalStatus::Failed { .. } => Color::Red,
    }
}

fn status_label(status: &TerminalStatus) -> String {
    match status {
        TerminalStatus::Starting => text(Text::StartingStatus).to_owned(),
        TerminalStatus::Running => text(Text::RunningStatus).to_owned(),
        TerminalStatus::Stopping => text(Text::StoppingStatus).to_owned(),
        TerminalStatus::Exited { code, signal } => signal.as_ref().map_or_else(
            || format!("{} {code}", text(Text::ExitedStatus)),
            |signal| format!("{} ({code})", sanitize_for_display(signal)),
        ),
        TerminalStatus::Failed { failure } => terminal_failure_text(failure),
    }
}

pub(crate) fn terminal_notice_text(notice: &TerminalNotice) -> String {
    let terminal = |id| format!("{} {id}", text(Text::Terminal));
    match notice {
        TerminalNotice::Disabled => text(Text::TerminalDisabledNotice).to_owned(),
        TerminalNotice::LimitReached { max_sessions } => format!(
            "{} · {}: {max_sessions}",
            text(Text::Terminals),
            text(Text::Max)
        ),
        TerminalNotice::Starting { id, cwd } => format!(
            "{} · {} · {}",
            terminal(*id),
            text(Text::StartingStatus),
            sanitize_for_display(&cwd.to_string_lossy())
        ),
        TerminalNotice::Missing { id } => {
            format!("{} · {}", terminal(*id), text(Text::Unavailable))
        }
        TerminalNotice::NotAcceptingInput { id } => format!(
            "{} · {} · {}",
            terminal(*id),
            text(Text::Input),
            text(Text::Unavailable)
        ),
        TerminalNotice::InputBackpressured { id } => format!(
            "{} · {} · {}",
            terminal(*id),
            text(Text::Input),
            text(Text::SubmitInProgress)
        ),
        TerminalNotice::Closed { id } => {
            format!("{} · {}", terminal(*id), text(Text::ClosedStatus))
        }
        TerminalNotice::ResizeFailed { id, detail }
        | TerminalNotice::StartFailed { id, detail }
        | TerminalNotice::ReapFailed { id, detail }
        | TerminalNotice::StopFailed { id, detail } => format!(
            "{} · {}: {}",
            terminal(*id),
            text(Text::FailedStatus),
            sanitize_for_display(detail)
        ),
        TerminalNotice::Ready { id } => {
            format!("{} · {}", terminal(*id), text(Text::Ready))
        }
        TerminalNotice::ParserFailed { id } => {
            format!("{} · {}", terminal(*id), text(Text::ParseError))
        }
        TerminalNotice::OutputClosed { id, detail } => format!(
            "{} · {} · {}: {}",
            terminal(*id),
            text(Text::OutputLabel),
            text(Text::ClosedStatus),
            sanitize_for_display(detail)
        ),
        TerminalNotice::InputFailed { id } => format!(
            "{} · {} · {}",
            terminal(*id),
            text(Text::Input),
            text(Text::FailedStatus)
        ),
        TerminalNotice::Exited { id, code } => {
            format!("{} · {} {code}", terminal(*id), text(Text::ExitedStatus))
        }
        TerminalNotice::StopAfterStartup { id } | TerminalNotice::Stopping { id } => {
            format!("{} · {}", terminal(*id), text(Text::StoppingStatus))
        }
    }
}

fn terminal_failure_text(failure: &TerminalFailure) -> String {
    match failure {
        TerminalFailure::InputUnavailable | TerminalFailure::InputClosed => {
            format!("{} · {}", text(Text::Input), text(Text::Unavailable))
        }
        TerminalFailure::ParserResize | TerminalFailure::ParserOutput => {
            text(Text::ParseError).to_owned()
        }
        TerminalFailure::Start { detail }
        | TerminalFailure::Reap { detail }
        | TerminalFailure::Stop { detail } => format!(
            "{}: {}",
            text(Text::FailedStatus),
            sanitize_for_display(detail)
        ),
        TerminalFailure::Input { detail } => format!(
            "{} · {}: {}",
            text(Text::Input),
            text(Text::FailedStatus),
            sanitize_for_display(detail)
        ),
    }
}

pub(crate) fn terminal_control_error_text(error: &TerminalControlError) -> String {
    match error {
        TerminalControlError::Busy => text(Text::SubmitInProgress).to_owned(),
        TerminalControlError::Closed => text(Text::ClosedStatus).to_owned(),
        TerminalControlError::InputTooLarge { max_bytes } => format!(
            "{} · {}: {max_bytes} {}",
            text(Text::Input),
            text(Text::Max),
            text(Text::Bytes)
        ),
    }
}

fn ratatui_style(style: TerminalStyle) -> Style {
    let mut result = Style::default()
        .fg(ratatui_color(style.foreground))
        .bg(ratatui_color(style.background));
    if style.bold {
        result = result.add_modifier(Modifier::BOLD);
    }
    if style.dim {
        result = result.add_modifier(Modifier::DIM);
    }
    if style.italic {
        result = result.add_modifier(Modifier::ITALIC);
    }
    if style.underline {
        result = result.add_modifier(Modifier::UNDERLINED);
    }
    if style.inverse {
        result = result.add_modifier(Modifier::REVERSED);
    }
    result
}

const fn ratatui_color(color: TerminalColor) -> Color {
    match color {
        TerminalColor::Default => Color::Reset,
        TerminalColor::Indexed(index) => Color::Indexed(index),
        TerminalColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

#[must_use]
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.as_bytes().to_vec();
    }
    let mut bytes = Vec::with_capacity(text.len().saturating_add(12));
    bytes.extend_from_slice(b"\x1b[200~");
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

#[must_use]
pub fn encode_key(key: KeyEvent, application_cursor: bool) -> Option<Vec<u8>> {
    let modifiers = key.modifiers;
    let sequence = match key.code {
        KeyCode::Char(character) if modifiers.contains(KeyModifiers::CONTROL) => {
            let upper = character.to_ascii_uppercase();
            if ('@'..='_').contains(&upper) {
                vec![(upper as u8) & 0x1f]
            } else if character == ' ' {
                vec![0]
            } else if character == '?' {
                vec![0x7f]
            } else {
                return None;
            }
        }
        KeyCode::Char(character) => character.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_sequence('A', modifiers, application_cursor),
        KeyCode::Down => cursor_sequence('B', modifiers, application_cursor),
        KeyCode::Right => cursor_sequence('C', modifiers, application_cursor),
        KeyCode::Left => cursor_sequence('D', modifiers, application_cursor),
        KeyCode::Home => cursor_sequence('H', modifiers, application_cursor),
        KeyCode::End => cursor_sequence('F', modifiers, application_cursor),
        KeyCode::Insert => modified_csi_tilde(2, modifiers),
        KeyCode::Delete => modified_csi_tilde(3, modifiers),
        KeyCode::PageUp => modified_csi_tilde(5, modifiers),
        KeyCode::PageDown => modified_csi_tilde(6, modifiers),
        KeyCode::F(number) => function_key(number, modifiers)?,
        _ => return None,
    };
    if modifiers.contains(KeyModifiers::ALT)
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
        )
    {
        let mut prefixed = Vec::with_capacity(sequence.len().saturating_add(1));
        prefixed.push(0x1b);
        prefixed.extend(sequence);
        Some(prefixed)
    } else {
        Some(sequence)
    }
}

fn cursor_sequence(code: char, modifiers: KeyModifiers, application_cursor: bool) -> Vec<u8> {
    let modifier = xterm_modifier(modifiers);
    if modifier == 1 {
        if application_cursor {
            format!("\x1bO{code}").into_bytes()
        } else {
            format!("\x1b[{code}").into_bytes()
        }
    } else {
        format!("\x1b[1;{modifier}{code}").into_bytes()
    }
}

fn modified_csi_tilde(number: u8, modifiers: KeyModifiers) -> Vec<u8> {
    let modifier = xterm_modifier(modifiers);
    if modifier == 1 {
        format!("\x1b[{number}~").into_bytes()
    } else {
        format!("\x1b[{number};{modifier}~").into_bytes()
    }
}

fn function_key(number: u8, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let modifier = xterm_modifier(modifiers);
    let base = match number {
        1 => "P",
        2 => "Q",
        3 => "R",
        4 => "S",
        5 => "15",
        6 => "17",
        7 => "18",
        8 => "19",
        9 => "20",
        10 => "21",
        11 => "23",
        12 => "24",
        _ => return None,
    };
    if number <= 4 {
        Some(if modifier == 1 {
            format!("\x1bO{base}").into_bytes()
        } else {
            format!("\x1b[1;{modifier}{base}").into_bytes()
        })
    } else {
        Some(if modifier == 1 {
            format!("\x1b[{base}~").into_bytes()
        } else {
            format!("\x1b[{base};{modifier}~").into_bytes()
        })
    }
}

const fn xterm_modifier(modifiers: KeyModifiers) -> u8 {
    1 + if modifiers.contains(KeyModifiers::SHIFT) {
        1
    } else {
        0
    } + if modifiers.contains(KeyModifiers::ALT) {
        2
    } else {
        0
    } + if modifiers.contains(KeyModifiers::CONTROL) {
        4
    } else {
        0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::SystemTime};

    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    use super::{
        TerminalFocus, TerminalHit, TerminalUiState, encode_key, encode_mouse, encode_paste,
    };
    use crate::terminal::{
        TerminalFleetSnapshot, TerminalFrame, TerminalMouseEncoding, TerminalMouseMode,
        TerminalSessionSnapshot, TerminalStatus,
    };

    #[test]
    fn key_encoder_handles_control_navigation_and_application_cursor() {
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode_key(KeyEvent::new(KeyCode::Left, KeyModifiers::CONTROL), false),
            Some(b"\x1b[1;5D".to_vec())
        );
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('['), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![0x1b])
        );
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![0x1c])
        );
        assert_eq!(
            encode_key(
                KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
                false
            ),
            Some(vec![0x1f])
        );
    }

    #[test]
    fn bracketed_paste_wraps_the_original_utf8_once() {
        assert_eq!(
            encode_paste("one\nдва", true),
            b"\x1b[200~one\n\xd0\xb4\xd0\xb2\xd0\xb0\x1b[201~".to_vec()
        );
        assert_eq!(encode_paste("a\r\nb", false), b"a\r\nb".to_vec());
    }

    #[test]
    fn sgr_mouse_encoding_is_relative_and_respects_protocol_mode() {
        let area = Rect::new(10, 5, 20, 10);
        let press = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 11,
            row: 7,
            modifiers: KeyModifiers::CONTROL,
        };
        assert_eq!(
            encode_mouse(
                press,
                area,
                TerminalMouseMode::PressRelease,
                TerminalMouseEncoding::Sgr,
            ),
            Some(b"\x1b[<16;2;3M".to_vec())
        );
        let movement = MouseEvent {
            kind: MouseEventKind::Moved,
            ..press
        };
        assert_eq!(
            encode_mouse(
                movement,
                area,
                TerminalMouseMode::Press,
                TerminalMouseEncoding::Sgr,
            ),
            None
        );
        let outside = MouseEvent { column: 2, ..press };
        assert_eq!(
            encode_mouse(
                outside,
                area,
                TerminalMouseMode::AnyMotion,
                TerminalMouseEncoding::Sgr,
            ),
            None
        );
    }

    #[test]
    fn terminal_controls_and_session_tab_are_real_click_regions() {
        let frame = Arc::new(TerminalFrame {
            rows: 8,
            cols: 40,
            cursor_row: 0,
            cursor_col: 0,
            hide_cursor: false,
            application_cursor: false,
            bracketed_paste: false,
            alternate_screen: false,
            mouse_mode: crate::terminal::TerminalMouseMode::None,
            mouse_encoding: crate::terminal::TerminalMouseEncoding::Default,
            scrollback_offset: 4,
            content: Arc::from([]),
        });
        let fleet = TerminalFleetSnapshot {
            revision: 1,
            enabled: true,
            max_sessions: 6,
            sessions: Arc::from([TerminalSessionSnapshot {
                id: 7,
                title: "Terminal 7".to_owned(),
                cwd: PathBuf::from("workspace"),
                created_at: SystemTime::UNIX_EPOCH,
                status: TerminalStatus::Running,
                process_id: Some(42),
                output_revision: 3,
                frame,
            }]),
            notice: None,
        };
        let mut ui = TerminalUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| ui.draw_tab(frame, frame.area(), &fleet))
            .unwrap();
        let area = terminal.backend().buffer().area;
        let mut hits = Vec::new();
        for row in 0..area.height {
            for column in 0..area.width {
                if let Some(hit) = ui.clicked(column, row) {
                    hits.push(hit);
                }
            }
        }
        assert!(hits.contains(&TerminalHit::New));
        assert!(hits.contains(&TerminalHit::Stop));
        assert!(hits.contains(&TerminalHit::Close));
        assert!(hits.contains(&TerminalHit::Latest));
        assert!(hits.contains(&TerminalHit::Session(7)));
        assert!(hits.contains(&TerminalHit::Screen));

        ui.toggle_input_mode();
        for _ in 0..4 {
            ui.next_control();
        }
        assert_eq!(ui.selected_control(), Some(TerminalFocus::Session(7)));
        assert!(
            ui.activate_control(TerminalFocus::Session(7), &fleet)
                .is_ok()
        );
        assert_eq!(ui.selected_id(), Some(7));
    }

    #[tokio::test]
    async fn failed_resize_is_kept_for_retry() {
        let workspace = tempfile::tempdir().unwrap();
        let (control, _snapshots, task) = crate::terminal::start_terminal_runtime(
            crate::config::InteractiveTerminalConfig::default(),
            workspace.path().to_path_buf(),
        );
        task.abort();
        let _ = task.await;
        let mut ui = TerminalUiState::new();
        ui.attach_control(control);
        ui.pending_resize = Some((7, 24, 80));

        assert!(matches!(
            ui.flush_pending_resize(),
            Err(crate::terminal::TerminalControlError::Closed)
        ));
        assert_eq!(ui.pending_resize, Some((7, 24, 80)));
    }

    #[test]
    fn removing_a_session_discards_its_pending_resize() {
        let mut ui = TerminalUiState::new();
        ui.pending_resize = Some((7, 24, 80));

        ui.sync(&TerminalFleetSnapshot::default());

        assert_eq!(ui.pending_resize, None);
    }
}
