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
        DialogConfig, DialogState, ListPicker, ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};

use crate::lsp::{LspConnectionState, LspDiagnostic, LspDiagnosticSeverity, LspServerSnapshot};

use super::{
    connections::{ConnectionEditor, ConnectionKind},
    i18n::{Text, notice_text, text},
    render::sanitize_for_display,
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspPane {
    Servers,
    Diagnostics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspFocus {
    ServersTab,
    DiagnosticsTab,
    Items,
    Close,
    Toggle,
    Primary,
    Stop,
    Refresh,
    Mention,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LspHit {
    ServersTab,
    DiagnosticsTab,
    ServerItem(usize),
    DiagnosticItem(usize),
    Close,
    Toggle,
    Primary,
    Stop,
    Refresh,
    Mention,
    Add,
}

#[derive(Debug, Clone)]
pub struct LspUiState {
    open: bool,
    pane: LspPane,
    dialog: DialogState<()>,
    server_picker: ListPickerState,
    diagnostic_picker: ListPickerState,
    selected_server_name: Option<String>,
    selected_diagnostic: Option<LspDiagnostic>,
    focus: FocusManager<LspFocus>,
    clicks: ClickRegionRegistry<LspHit>,
    animation_frame: usize,
    last_animation_at: Instant,
    editor: ConnectionEditor,
}

impl LspUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            LspFocus::ServersTab,
            LspFocus::DiagnosticsTab,
            LspFocus::Items,
            LspFocus::Close,
            LspFocus::Toggle,
            LspFocus::Primary,
            LspFocus::Stop,
            LspFocus::Refresh,
            LspFocus::Mention,
            LspFocus::Add,
        ] {
            focus.register(item);
        }
        focus.set(LspFocus::Items);
        Self {
            open: false,
            pane: LspPane::Servers,
            dialog: DialogState::new(()),
            server_picker: ListPickerState::new(0),
            diagnostic_picker: ListPickerState::new(0),
            selected_server_name: None,
            selected_diagnostic: None,
            focus,
            clicks: ClickRegionRegistry::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
            editor: ConnectionEditor::new(ConnectionKind::Lsp),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    #[must_use]
    pub const fn pane(&self) -> LspPane {
        self.pane
    }

    pub fn open(&mut self, servers: usize, diagnostics: usize) {
        self.open = true;
        self.set_counts(servers, diagnostics);
        self.focus.set(LspFocus::Items);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        if self.editor.is_open() {
            self.editor.close();
            return;
        }
        self.open = false;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
        self.editor.begin_frame();
    }

    pub fn tick(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_animation_at) >= ANIMATION_STEP {
            self.animation_frame = self.animation_frame.wrapping_add(1);
            self.last_animation_at = now;
        }
    }

    #[must_use]
    pub const fn is_editing(&self) -> bool {
        self.editor.is_open()
    }

    pub fn open_editor(&mut self) {
        self.editor.open();
    }

    pub fn editor(&self) -> &ConnectionEditor {
        &self.editor
    }

    pub fn editor_mut(&mut self) -> &mut ConnectionEditor {
        &mut self.editor
    }

    pub fn set_counts(&mut self, servers: usize, diagnostics: usize) {
        self.server_picker.set_total(servers);
        self.diagnostic_picker.set_total(diagnostics);
        if servers > 0 && self.server_picker.selected_index >= servers {
            self.server_picker.select(servers.saturating_sub(1));
        }
        if diagnostics > 0 && self.diagnostic_picker.selected_index >= diagnostics {
            self.diagnostic_picker.select(diagnostics.saturating_sub(1));
        }
    }

    pub fn sync(&mut self, servers: &[LspServerSnapshot], diagnostics: &[LspDiagnostic]) {
        self.clicks.clear();
        if let Some(name) = self.selected_server_name.as_deref()
            && let Some(index) = servers.iter().position(|server| server.name == name)
        {
            self.server_picker.select(index);
        }
        if let Some(selected) = self.selected_diagnostic.as_ref()
            && let Some(index) = diagnostics
                .iter()
                .position(|diagnostic| diagnostic == selected)
        {
            self.diagnostic_picker.select(index);
        }
        self.set_counts(servers.len(), diagnostics.len());
        self.selected_server_name = servers
            .get(self.server_picker.selected_index)
            .map(|server| server.name.clone());
        self.selected_diagnostic = diagnostics
            .get(self.diagnostic_picker.selected_index)
            .cloned();
    }

    pub fn set_pane(&mut self, pane: LspPane) {
        self.pane = pane;
        self.focus.set(LspFocus::Items);
    }

    #[must_use]
    pub const fn selected_server(&self) -> usize {
        self.server_picker.selected_index
    }

    #[must_use]
    pub const fn selected_diagnostic(&self) -> usize {
        self.diagnostic_picker.selected_index
    }

    pub fn select_server(&mut self, index: usize) {
        self.server_picker.select(index);
        self.selected_server_name = None;
    }

    pub fn select_diagnostic(&mut self, index: usize) {
        self.diagnostic_picker.select(index);
        self.selected_diagnostic = None;
    }

    pub fn next_item(&mut self) {
        match self.pane {
            LspPane::Servers => {
                self.server_picker.select_next();
                self.selected_server_name = None;
            }
            LspPane::Diagnostics => {
                self.diagnostic_picker.select_next();
                self.selected_diagnostic = None;
            }
        }
    }

    pub fn previous_item(&mut self) {
        match self.pane {
            LspPane::Servers => {
                self.server_picker.select_prev();
                self.selected_server_name = None;
            }
            LspPane::Diagnostics => {
                self.diagnostic_picker.select_prev();
                self.selected_diagnostic = None;
            }
        }
    }

    pub fn first_item(&mut self) {
        match self.pane {
            LspPane::Servers => {
                self.server_picker.select_first();
                self.selected_server_name = None;
            }
            LspPane::Diagnostics => {
                self.diagnostic_picker.select_first();
                self.selected_diagnostic = None;
            }
        }
    }

    pub fn last_item(&mut self) {
        match self.pane {
            LspPane::Servers => {
                self.server_picker.select_last();
                self.selected_server_name = None;
            }
            LspPane::Diagnostics => {
                self.diagnostic_picker.select_last();
                self.selected_diagnostic = None;
            }
        }
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: LspFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<LspFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<LspHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        servers: &[LspServerSnapshot],
        diagnostics: &[LspDiagnostic],
        editable: bool,
    ) {
        if !self.open {
            return;
        }
        self.sync(servers, diagnostics);
        let config = DialogConfig::new(text(Text::LanguageIntelligence))
            .width_percent(88)
            .height_percent(80)
            .min_size(72, 20)
            .max_size(170, 58)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let pane = self.pane;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let selected_server = self.server_picker.selected_index;
        let selected_diagnostic = self.diagnostic_picker.selected_index;
        let server_picker = &mut self.server_picker;
        let diagnostic_picker = &mut self.diagnostic_picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                servers,
                diagnostics,
                pane,
                focused,
                animation_frame,
                selected_server,
                selected_diagnostic,
                editable,
                server_picker,
                diagnostic_picker,
                clicks,
            );
        });
        popup.render(frame);
        self.editor.draw(frame);
    }
}

impl Default for LspUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    servers: &[LspServerSnapshot],
    diagnostics: &[LspDiagnostic],
    pane: LspPane,
    focused: Option<LspFocus>,
    animation_frame: usize,
    selected_server: usize,
    selected_diagnostic: usize,
    editable: bool,
    server_picker: &mut ListPickerState,
    diagnostic_picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(9),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(text(Text::LspReadOnlyHelp)).wrap(Wrap { trim: false }),
        rows[0],
    );
    draw_pane_tabs(frame, rows[1], pane, focused, clicks);
    match pane {
        LspPane::Servers => draw_servers(
            frame,
            rows[2],
            servers,
            selected_server,
            animation_frame,
            server_picker,
            clicks,
        ),
        LspPane::Diagnostics => draw_diagnostics(
            frame,
            rows[2],
            diagnostics,
            selected_diagnostic,
            diagnostic_picker,
            clicks,
        ),
    }
    draw_add(frame, rows[3], pane, focused, editable, clicks);
    draw_actions(
        frame,
        rows[4],
        pane,
        servers.get(selected_server),
        diagnostics.get(selected_diagnostic),
        focused,
        editable,
        clicks,
    );
}

fn draw_add(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: LspPane,
    focused: Option<LspFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(32),
        Constraint::Fill(1),
    ])
    .split(area);
    render_button(
        frame,
        columns[1],
        text(Text::AddLanguageServer),
        LspHit::Add,
        focused == Some(LspFocus::Add),
        pane == LspPane::Servers && editable,
        ButtonStyle::primary(),
        clicks,
    );
}

fn draw_pane_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: LspPane,
    focused: Option<LspFocus>,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(24),
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Fill(1),
    ])
    .split(area);
    render_button(
        frame,
        columns[1],
        text(Text::ServersLabel),
        LspHit::ServersTab,
        focused == Some(LspFocus::ServersTab),
        true,
        if pane == LspPane::Servers {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        },
        clicks,
    );
    render_button(
        frame,
        columns[3],
        text(Text::DiagnosticsLabel),
        LspHit::DiagnosticsTab,
        focused == Some(LspFocus::DiagnosticsTab),
        true,
        if pane == LspPane::Diagnostics {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        },
        clicks,
    );
}

fn draw_servers(
    frame: &mut Frame<'_>,
    area: Rect,
    servers: &[LspServerSnapshot],
    selected: usize,
    animation_frame: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(43), Constraint::Percentage(57)]).split(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ServersLabel)));
    let inner = block.inner(columns[0]);
    frame.render_widget(block, columns[0]);
    let labels = servers
        .iter()
        .map(|server| {
            format!(
                "{} {}  · {} {}",
                state_icon(server.state, animation_frame),
                sanitize_for_display(&server.name),
                server.diagnostic_count,
                text(Text::DiagnosticsCountLabel)
            )
        })
        .collect::<Vec<_>>();
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= servers.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            LspHit::ServerItem(index),
        );
    }

    let detail = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ServerDetails)));
    let detail_inner = detail.inner(columns[1]);
    frame.render_widget(detail, columns[1]);
    let lines = servers.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoLanguageServers))],
        |server| {
            vec![
                Line::from(Span::styled(
                    format!(
                        "{} {}",
                        state_icon(server.state, animation_frame),
                        sanitize_for_display(&server.name)
                    ),
                    Style::default()
                        .fg(state_color(server.state))
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "{}: {}",
                    text(Text::LanguageIdLabel),
                    sanitize_for_display(&server.language_id)
                )),
                Line::from(format!(
                    "{}: {}  · {}: {}  · {}: {}",
                    text(Text::DetectedLabel),
                    yes_no(server.detected),
                    text(Text::AutoStartLabel),
                    yes_no(server.auto_start),
                    text(Text::PolicyLabel),
                    if server.required {
                        text(Text::RequiredLabel)
                    } else {
                        text(Text::OptionalLabel)
                    }
                )),
                Line::from(format!(
                    "{}: {}  |  {}: {}",
                    text(Text::GlobalRuntimeLabel),
                    if server.runtime_available {
                        text(Text::AvailableLabel)
                    } else {
                        text(Text::DisabledTrustedConfig)
                    },
                    text(Text::ServerSwitchLabel),
                    if server.enabled {
                        text(Text::EnabledLabel)
                    } else {
                        text(Text::DisabledLabel)
                    }
                )),
                Line::from(format!(
                    "{}: {}",
                    text(Text::DiagnosticsLabel),
                    server.diagnostic_count
                )),
                Line::from(format!(
                    "{}: {}",
                    text(Text::Status),
                    sanitize_for_display(&lsp_status_text(server))
                )),
                Line::from(""),
                Line::from(text(Text::LspBoundedHelp)),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        detail_inner,
    );
}

fn lsp_status_text(server: &LspServerSnapshot) -> String {
    let notice = notice_text(&server.notice);
    if !notice.is_empty() {
        return notice;
    }
    text(match server.state {
        LspConnectionState::Disabled => Text::DisabledLabel,
        LspConnectionState::NotDetected => Text::Unavailable,
        LspConnectionState::Disconnected => Text::ClosedStatus,
        LspConnectionState::Starting => Text::StartingStatus,
        LspConnectionState::Connected => Text::Ready,
        LspConnectionState::Error => Text::FailedStatus,
    })
    .to_owned()
}

fn draw_diagnostics(
    frame: &mut Frame<'_>,
    area: Rect,
    diagnostics: &[LspDiagnostic],
    selected: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} ({}) ",
        text(Text::DiagnosticsLabel),
        diagnostics.len()
    ));
    let inner = block.inner(columns[0]);
    frame.render_widget(block, columns[0]);
    let labels = diagnostics
        .iter()
        .map(|diagnostic| {
            sanitize_for_display(&format!(
                "{} {}:{}:{}  {}",
                severity_icon(diagnostic.severity),
                diagnostic.path,
                diagnostic.line,
                diagnostic.column,
                diagnostic.message.lines().next().unwrap_or_default()
            ))
        })
        .collect::<Vec<_>>();
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= diagnostics.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            LspHit::DiagnosticItem(index),
        );
    }

    let detail = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::DiagnosticDetails)));
    let detail_inner = detail.inner(columns[1]);
    frame.render_widget(detail, columns[1]);
    let lines = diagnostics.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoDiagnosticsPublished))],
        |diagnostic| {
            vec![
                Line::from(Span::styled(
                    format!(
                        "{} {}",
                        severity_icon(diagnostic.severity),
                        severity_label(diagnostic.severity)
                    ),
                    Style::default()
                        .fg(severity_color(diagnostic.severity))
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "{}:{}:{}–{}:{}",
                    sanitize_for_display(&diagnostic.path),
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.end_line,
                    diagnostic.end_column
                )),
                Line::from(format!(
                    "{}: {}",
                    text(Text::ServerLabel),
                    sanitize_for_display(&diagnostic.server)
                )),
                Line::from(format!(
                    "{}: {} / {}",
                    text(Text::SourceCodeLabel),
                    sanitize_for_display(diagnostic.source.as_deref().unwrap_or("-")),
                    sanitize_for_display(diagnostic.code.as_deref().unwrap_or("-"))
                )),
                Line::from(""),
                Line::from(sanitize_for_display(&diagnostic.message)),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        detail_inner,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: LspPane,
    server: Option<&LspServerSnapshot>,
    diagnostic: Option<&LspDiagnostic>,
    focused: Option<LspFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Length(14),
        Constraint::Length(23),
        Constraint::Fill(1),
        Constraint::Length(18),
        Constraint::Length(16),
        Constraint::Length(16),
        Constraint::Length(22),
    ])
    .split(area);
    render_button(
        frame,
        columns[0],
        text(Text::Close),
        LspHit::Close,
        focused == Some(LspFocus::Close),
        true,
        ButtonStyle::default(),
        clicks,
    );
    render_toggle(
        frame,
        columns[1],
        (pane == LspPane::Servers).then_some(server).flatten(),
        focused == Some(LspFocus::Toggle),
        editable,
        clicks,
    );
    let primary_enabled = editable
        && pane == LspPane::Servers
        && server
            .is_some_and(|server| server.runtime_available && server.enabled && server.detected)
        && !server.is_some_and(|server| server.state == LspConnectionState::Starting);
    let primary_label =
        if server.is_some_and(|server| server.state == LspConnectionState::Connected) {
            text(Text::RestartLabel)
        } else {
            text(Text::StartLabel)
        };
    render_button(
        frame,
        columns[3],
        primary_label,
        LspHit::Primary,
        focused == Some(LspFocus::Primary),
        primary_enabled,
        ButtonStyle::primary(),
        clicks,
    );
    let stop_enabled = editable
        && pane == LspPane::Servers
        && server.is_some_and(|server| {
            matches!(
                server.state,
                LspConnectionState::Starting | LspConnectionState::Connected
            )
        });
    render_button(
        frame,
        columns[4],
        text(Text::StopLabel),
        LspHit::Stop,
        focused == Some(LspFocus::Stop),
        stop_enabled,
        ButtonStyle::danger(),
        clicks,
    );
    render_button(
        frame,
        columns[5],
        text(Text::RefreshLabel),
        LspHit::Refresh,
        focused == Some(LspFocus::Refresh),
        editable,
        ButtonStyle::default(),
        clicks,
    );
    render_button(
        frame,
        columns[6],
        text(Text::MentionInChat),
        LspHit::Mention,
        focused == Some(LspFocus::Mention),
        pane == LspPane::Diagnostics && diagnostic.is_some(),
        ButtonStyle::primary(),
        clicks,
    );
}

fn render_toggle(
    frame: &mut Frame<'_>,
    area: Rect,
    server: Option<&LspServerSnapshot>,
    focused: bool,
    editable: bool,
    clicks: &mut ClickRegionRegistry<LspHit>,
) {
    let enabled = editable && server.is_some_and(|server| server.runtime_available);
    let mut state = CheckBoxState::new(server.is_some_and(|server| server.enabled));
    state.set_enabled(enabled);
    state.set_focused(focused);
    let toggle_area = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
    let region = CheckBox::new(text(Text::EnabledThisRun), &state)
        .style(
            CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(toggle_area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, LspHit::Toggle);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: LspHit,
    focused: bool,
    enabled: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<LspHit>,
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

fn state_icon(state: LspConnectionState, animation_frame: usize) -> &'static str {
    match state {
        LspConnectionState::Connected => "●",
        LspConnectionState::Starting => ["◐", "◓", "◑", "◒"][animation_frame % 4],
        LspConnectionState::Error => "!",
        LspConnectionState::Disconnected => "○",
        LspConnectionState::NotDetected => "◇",
        LspConnectionState::Disabled => "–",
    }
}

const fn state_color(state: LspConnectionState) -> Color {
    match state {
        LspConnectionState::Connected => Color::Green,
        LspConnectionState::Starting => Color::LightCyan,
        LspConnectionState::Error => Color::Red,
        LspConnectionState::Disconnected => Color::Gray,
        LspConnectionState::NotDetected => Color::Yellow,
        LspConnectionState::Disabled => Color::DarkGray,
    }
}

const fn severity_icon(severity: LspDiagnosticSeverity) -> &'static str {
    match severity {
        LspDiagnosticSeverity::Error => "×",
        LspDiagnosticSeverity::Warning => "△",
        LspDiagnosticSeverity::Information => "i",
        LspDiagnosticSeverity::Hint => "·",
        LspDiagnosticSeverity::Unknown => "?",
    }
}

const fn severity_color(severity: LspDiagnosticSeverity) -> Color {
    match severity {
        LspDiagnosticSeverity::Error => Color::Red,
        LspDiagnosticSeverity::Warning => Color::Yellow,
        LspDiagnosticSeverity::Information => Color::LightBlue,
        LspDiagnosticSeverity::Hint => Color::Cyan,
        LspDiagnosticSeverity::Unknown => Color::Gray,
    }
}

fn severity_label(severity: LspDiagnosticSeverity) -> &'static str {
    match severity {
        LspDiagnosticSeverity::Error => text(Text::ErrorLabel),
        LspDiagnosticSeverity::Warning => text(Text::WarningLabel),
        LspDiagnosticSeverity::Information => text(Text::InformationLabel),
        LspDiagnosticSeverity::Hint => text(Text::HintLabel),
        LspDiagnosticSeverity::Unknown => text(Text::UnknownLabel),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        text(Text::YesLabel)
    } else {
        text(Text::NoLabel)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        LspConnectionState, LspDiagnostic, LspDiagnosticSeverity, LspFocus, LspHit, LspPane,
        LspServerSnapshot, LspUiState, state_icon,
    };

    fn assert_click_region(ui: &LspUiState, area: ratatui::layout::Rect, expected: LspHit) {
        let found = (0..area.height)
            .any(|row| (0..area.width).any(|column| ui.clicked(column, row) == Some(expected)));
        assert!(found, "missing click region for {expected:?}");
    }

    fn server(state: LspConnectionState) -> LspServerSnapshot {
        LspServerSnapshot {
            name: "rust-analyzer".to_owned(),
            language_id: "rust".to_owned(),
            runtime_available: true,
            enabled: true,
            required: false,
            auto_start: true,
            detected: true,
            state,
            diagnostic_count: 1,
            notice: crate::notice::UiNotice::LspReady,
        }
    }

    fn diagnostic() -> LspDiagnostic {
        LspDiagnostic {
            server: "rust-analyzer".to_owned(),
            path: "src/main.rs".to_owned(),
            line: 7,
            column: 3,
            end_line: 7,
            end_column: 8,
            severity: LspDiagnosticSeverity::Error,
            message: "unknown value".to_owned(),
            source: Some("rustc".to_owned()),
            code: Some("E0001".to_owned()),
        }
    }

    #[test]
    fn starting_indicator_animates_but_connected_is_stable() {
        assert_ne!(
            state_icon(LspConnectionState::Starting, 0),
            state_icon(LspConnectionState::Starting, 1)
        );
        assert_eq!(
            state_icon(LspConnectionState::Connected, 0),
            state_icon(LspConnectionState::Connected, 100)
        );
    }

    #[test]
    fn every_enabled_server_action_has_a_mouse_path() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = LspUiState::new();
        ui.open(1, 0);
        let mut terminal = Terminal::new(TestBackend::new(160, 50))?;
        terminal.draw(|frame| {
            ui.draw(frame, &[server(LspConnectionState::Connected)], &[], true);
        })?;
        let area = terminal.backend().buffer().area;
        for expected in [
            LspHit::ServersTab,
            LspHit::DiagnosticsTab,
            LspHit::ServerItem(0),
            LspHit::Close,
            LspHit::Toggle,
            LspHit::Primary,
            LspHit::Stop,
            LspHit::Refresh,
            LspHit::Add,
        ] {
            assert_click_region(&ui, area, expected);
        }
        Ok(())
    }

    #[test]
    fn diagnostic_can_be_selected_and_mentioned_by_mouse() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut ui = LspUiState::new();
        ui.open(1, 1);
        ui.set_pane(LspPane::Diagnostics);
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                &[server(LspConnectionState::Connected)],
                &[diagnostic()],
                true,
            );
        })?;
        let area = terminal.backend().buffer().area;
        for expected in [
            LspHit::ServersTab,
            LspHit::DiagnosticsTab,
            LspHit::DiagnosticItem(0),
            LspHit::Close,
            LspHit::Refresh,
            LspHit::Mention,
        ] {
            assert_click_region(&ui, area, expected);
        }
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_reach_every_lsp_control() {
        let mut ui = LspUiState::new();
        ui.open(1, 1);
        let expected = [
            LspFocus::Items,
            LspFocus::Close,
            LspFocus::Toggle,
            LspFocus::Primary,
            LspFocus::Stop,
            LspFocus::Refresh,
            LspFocus::Mention,
            LspFocus::Add,
            LspFocus::ServersTab,
            LspFocus::DiagnosticsTab,
            LspFocus::Items,
        ];
        assert_eq!(ui.focused(), Some(expected[0]));
        for focus in expected.iter().skip(1) {
            ui.next_focus();
            assert_eq!(ui.focused(), Some(*focus));
        }
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(LspFocus::DiagnosticsTab));
    }

    #[test]
    fn globally_disabled_runtime_has_no_mutating_click_regions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut unavailable = server(LspConnectionState::Disabled);
        unavailable.runtime_available = false;
        let mut ui = LspUiState::new();
        ui.open(1, 0);
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| ui.draw(frame, &[unavailable], &[], true))?;
        let buffer = terminal.backend().buffer();
        for forbidden in [LspHit::Toggle, LspHit::Primary, LspHit::Stop] {
            let found = (0..buffer.area.height).any(|row| {
                (0..buffer.area.width).any(|column| ui.clicked(column, row) == Some(forbidden))
            });
            if found {
                return Err(io::Error::other(format!(
                    "disabled runtime exposed click region for {forbidden:?}"
                ))
                .into());
            }
        }
        Ok(())
    }

    #[test]
    fn selections_follow_servers_and_diagnostics_across_reordering()
    -> Result<(), Box<dyn std::error::Error>> {
        let first_server = server(LspConnectionState::Connected);
        let mut second_server = first_server.clone();
        second_server.name = "second-server".to_owned();
        let first_diagnostic = diagnostic();
        let mut second_diagnostic = first_diagnostic.clone();
        second_diagnostic.path = "src/second.rs".to_owned();
        let mut servers = vec![first_server.clone(), second_server.clone()];
        let mut diagnostics = vec![first_diagnostic.clone(), second_diagnostic.clone()];
        let mut ui = LspUiState::new();
        ui.open(servers.len(), diagnostics.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| ui.draw(frame, &servers, &diagnostics, true))?;
        ui.select_server(1);
        ui.select_diagnostic(1);
        terminal.draw(|frame| ui.draw(frame, &servers, &diagnostics, true))?;

        servers.reverse();
        diagnostics.reverse();
        terminal.draw(|frame| ui.draw(frame, &servers, &diagnostics, true))?;

        assert_eq!(servers[ui.selected_server()].name, second_server.name);
        assert_eq!(
            diagnostics[ui.selected_diagnostic()].path,
            second_diagnostic.path
        );
        Ok(())
    }
}
