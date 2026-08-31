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

use crate::{
    agent::SubagentFleetSnapshot,
    mcp::{McpConnectionState, McpOAuthPrompt, McpServerSnapshot},
};

use super::{
    connections::{ConnectionEditor, ConnectionKind},
    i18n::{Text, notice_text, text},
    render::sanitize_for_display,
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpFocus {
    Close,
    Toggle,
    Primary,
    Secondary,
    Subagents,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McpHit {
    Item(usize),
    Close,
    Toggle,
    Primary,
    Secondary,
    Subagents,
    Add,
}

#[derive(Debug, Clone)]
pub struct McpUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    server_names: Vec<String>,
    selected_server_name: Option<String>,
    focus: FocusManager<McpFocus>,
    clicks: ClickRegionRegistry<McpHit>,
    animation_frame: usize,
    last_animation_at: Instant,
    editor: ConnectionEditor,
}

impl McpUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(McpFocus::Close);
        focus.register(McpFocus::Toggle);
        focus.register(McpFocus::Primary);
        focus.register(McpFocus::Secondary);
        focus.register(McpFocus::Subagents);
        focus.register(McpFocus::Add);
        focus.set(McpFocus::Primary);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            server_names: Vec::new(),
            selected_server_name: None,
            focus,
            clicks: ClickRegionRegistry::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
            editor: ConnectionEditor::new(ConnectionKind::Mcp),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, total: usize) {
        self.open = true;
        self.set_total(total);
        self.focus.set(McpFocus::Primary);
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

    pub fn set_total(&mut self, total: usize) {
        self.picker.set_total(total);
        if total > 0 && self.picker.selected_index >= total {
            self.picker.select(total.saturating_sub(1));
        }
    }

    pub fn sync(&mut self, servers: &[McpServerSnapshot]) {
        self.clicks.clear();
        if let Some(name) = self.selected_server_name.as_deref()
            && let Some(index) = servers.iter().position(|server| server.name == name)
        {
            self.picker.select(index);
        }
        self.set_total(servers.len());
        self.server_names = servers.iter().map(|server| server.name.clone()).collect();
        self.update_selected_server_name();
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.update_selected_server_name();
    }

    pub fn next(&mut self) {
        self.picker.select_next();
        self.update_selected_server_name();
    }

    pub fn previous(&mut self) {
        self.picker.select_prev();
        self.update_selected_server_name();
    }

    pub fn first(&mut self) {
        self.picker.select_first();
        self.update_selected_server_name();
    }

    pub fn last(&mut self) {
        self.picker.select_last();
        self.update_selected_server_name();
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: McpFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<McpFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<McpHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        servers: &[McpServerSnapshot],
        oauth_prompt: Option<&McpOAuthPrompt>,
        subagents: &SubagentFleetSnapshot,
        editable: bool,
    ) {
        if !self.open {
            return;
        }
        self.sync(servers);
        let selected = self.picker.selected_index;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let config = DialogConfig::new(text(Text::McpConnections))
            .width_percent(84)
            .height_percent(76)
            .min_size(68, 18)
            .max_size(160, 54)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                servers,
                oauth_prompt,
                subagents,
                selected,
                picker,
                focused,
                animation_frame,
                editable,
                clicks,
            );
        });
        popup.render(frame);
        self.editor.draw(frame);
    }

    fn update_selected_server_name(&mut self) {
        self.selected_server_name = self.server_names.get(self.picker.selected_index).cloned();
    }
}

impl Default for McpUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    servers: &[McpServerSnapshot],
    oauth_prompt: Option<&McpOAuthPrompt>,
    subagents: &SubagentFleetSnapshot,
    selected: usize,
    picker: &mut ListPickerState,
    focused: Option<McpFocus>,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<McpHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(text(Text::McpFiniteRetryHelp)).wrap(Wrap { trim: false }),
        rows[0],
    );

    let columns =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(rows[1]);
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ServersLabel)));
    let list_inner = list_block.inner(columns[0]);
    frame.render_widget(list_block, columns[0]);
    let labels = servers
        .iter()
        .map(|server| {
            format!(
                "{} {}  · {} {}",
                state_icon(server.state, animation_frame),
                sanitize_for_display(&server.name),
                server.tool_count,
                text(Text::McpToolsCountLabel)
            )
        })
        .collect::<Vec<_>>();
    let viewport = usize::from(list_inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        list_inner,
    );
    for visible_row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(visible_row);
        if index >= servers.len() {
            break;
        }
        clicks.register(
            Rect::new(
                list_inner.x,
                list_inner.y.saturating_add(visible_row as u16),
                list_inner.width,
                1,
            ),
            McpHit::Item(index),
        );
    }

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ConnectionDetailsTitle)));
    let detail_inner = detail_block.inner(columns[1]);
    frame.render_widget(detail_block, columns[1]);
    let details = servers.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoMcpServers))],
        |server| detail_lines(server, oauth_prompt, animation_frame),
    );
    frame.render_widget(
        Paragraph::new(details).wrap(Wrap { trim: false }),
        detail_inner,
    );

    render_subagent_toggle(frame, rows[2], subagents, focused, editable, clicks);
    render_add(frame, rows[3], focused, editable, clicks);
    draw_buttons(
        frame,
        rows[4],
        servers.get(selected),
        focused,
        editable,
        clicks,
    );
}

fn render_add(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<McpFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<McpHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(28),
        Constraint::Fill(1),
    ])
    .split(area);
    render_button(
        frame,
        columns[1],
        text(Text::AddMcpServer),
        McpHit::Add,
        focused == Some(McpFocus::Add),
        editable,
        ButtonStyle::primary(),
        clicks,
    );
}

fn render_subagent_toggle(
    frame: &mut Frame<'_>,
    area: Rect,
    subagents: &SubagentFleetSnapshot,
    focused: Option<McpFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<McpHit>,
) {
    let mut state = CheckBoxState::new(subagents.mcp_enabled);
    state.set_focused(focused == Some(McpFocus::Subagents));
    state.set_enabled(subagents.enabled && editable);
    let label = format!(
        "{} | {}",
        text(Text::AllowSubagentsMcp),
        sanitize_for_display(&notice_text(&subagents.mcp_status))
    );
    let region = CheckBox::new(&label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    if subagents.enabled && editable {
        clicks.register(region.area, McpHit::Subagents);
    }
}

fn detail_lines(
    server: &McpServerSnapshot,
    oauth_prompt: Option<&McpOAuthPrompt>,
    animation_frame: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
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
            text(Text::TransportLabel),
            sanitize_for_display(server.transport)
        )),
        Line::from(format!(
            "{}: {} · {}",
            text(Text::PolicyLabel),
            if server.required {
                text(Text::RequiredLabel)
            } else {
                text(Text::OptionalLabel)
            },
            if server.enabled {
                text(Text::EnabledForRun)
            } else {
                text(Text::DisabledForRun)
            }
        )),
        Line::from(if server.runtime_available {
            text(Text::GlobalMcpEnabledTrusted)
        } else {
            text(Text::GlobalMcpDisabledTrusted)
        }),
        Line::from(format!(
            "{}: {}",
            text(Text::ToolsAdvertised),
            server.tool_count
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::Status),
            sanitize_for_display(&mcp_status_text(server))
        )),
    ];
    if server.oauth {
        lines.push(Line::from(format!(
            "{}: {}",
            text(Text::AuthenticationLabel),
            text(Text::OAuthPkceKeyring)
        )));
    }
    if let Some(prompt) = oauth_prompt.filter(|prompt| prompt.server == server.name) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            text(Text::WaitingBrowserCallback),
            Style::default().fg(Color::LightCyan),
        )));
        lines.push(Line::from(format!(
            "{}: {}",
            text(Text::CallbackLabel),
            sanitize_for_display(&prompt.redirect_uri)
        )));
        if !prompt.browser_opened {
            lines.push(Line::from(text(Text::BrowserOpenFailed)));
        }
    }
    lines
}

fn mcp_status_text(server: &McpServerSnapshot) -> String {
    let notice = notice_text(&server.notice);
    if !notice.is_empty() {
        return notice;
    }
    text(match server.state {
        McpConnectionState::Disabled => Text::DisabledLabel,
        McpConnectionState::Disconnected => Text::ClosedStatus,
        McpConnectionState::Connecting | McpConnectionState::Reconnecting => {
            Text::ConnectingEllipsis
        }
        McpConnectionState::Connected => Text::Ready,
        McpConnectionState::ReauthRequired => Text::OAuthWaitingCallback,
        McpConnectionState::Error => Text::FailedStatus,
    })
    .to_owned()
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    server: Option<&McpServerSnapshot>,
    focused: Option<McpFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<McpHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Length(16),
        Constraint::Length(25),
        Constraint::Fill(1),
        Constraint::Length(22),
        Constraint::Length(1),
        Constraint::Length(20),
    ])
    .split(area);
    render_button(
        frame,
        columns[0],
        text(Text::CloseEsc),
        McpHit::Close,
        focused == Some(McpFocus::Close),
        true,
        ButtonStyle::default(),
        clicks,
    );
    render_toggle(
        frame,
        columns[1],
        server,
        focused == Some(McpFocus::Toggle),
        editable,
        clicks,
    );
    let (primary_label, primary_enabled) = server.map_or((text(Text::NoServer), false), |server| {
        if !server.runtime_available {
            (text(Text::McpDisabledGlobally), false)
        } else if !server.enabled {
            (text(Text::EnableSwitchFirst), false)
        } else if server.oauth && server.state == McpConnectionState::ReauthRequired {
            (text(Text::AuthorizeBrowser), true)
        } else if server.state == McpConnectionState::Connected {
            (text(Text::DisconnectLabel), true)
        } else if matches!(
            server.state,
            McpConnectionState::Connecting | McpConnectionState::Reconnecting
        ) {
            (text(Text::ConnectingEllipsis), false)
        } else {
            (text(Text::ConnectLabel), true)
        }
    });
    render_button(
        frame,
        columns[3],
        primary_label,
        McpHit::Primary,
        focused == Some(McpFocus::Primary),
        editable && primary_enabled,
        ButtonStyle::primary(),
        clicks,
    );
    let secondary_enabled = editable
        && server.is_some_and(|server| server.runtime_available && server.oauth && server.enabled);
    render_button(
        frame,
        columns[5],
        text(Text::ForgetOAuth),
        McpHit::Secondary,
        focused == Some(McpFocus::Secondary),
        secondary_enabled,
        ButtonStyle::danger(),
        clicks,
    );
}

fn render_toggle(
    frame: &mut Frame<'_>,
    area: Rect,
    server: Option<&McpServerSnapshot>,
    focused: bool,
    editable: bool,
    clicks: &mut ClickRegionRegistry<McpHit>,
) {
    let checked = server.is_some_and(|server| server.enabled);
    let enabled = editable && server.is_some_and(|server| server.runtime_available);
    let mut state = CheckBoxState::new(checked);
    state.set_focused(focused);
    state.set_enabled(enabled);
    let toggle_area = Rect::new(area.x, area.y.saturating_add(1), area.width, 1);
    let region = CheckBox::new(text(Text::EnabledThisRun), &state)
        .style(
            CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(toggle_area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, McpHit::Toggle);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: McpHit,
    focused: bool,
    enabled: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<McpHit>,
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

fn state_icon(state: McpConnectionState, animation_frame: usize) -> &'static str {
    match state {
        McpConnectionState::Connected => "●",
        McpConnectionState::Connecting | McpConnectionState::Reconnecting => {
            ["◐", "◓", "◑", "◒"][animation_frame % 4]
        }
        McpConnectionState::ReauthRequired => "◆",
        McpConnectionState::Error => "!",
        McpConnectionState::Disconnected => "◌",
        McpConnectionState::Disabled => "○",
    }
}

const fn state_color(state: McpConnectionState) -> Color {
    match state {
        McpConnectionState::Connected => Color::Green,
        McpConnectionState::Connecting | McpConnectionState::Reconnecting => Color::LightCyan,
        McpConnectionState::ReauthRequired => Color::Yellow,
        McpConnectionState::Error => Color::Red,
        McpConnectionState::Disconnected => Color::Gray,
        McpConnectionState::Disabled => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{agent::SubagentFleetSnapshot, mcp::McpConnectionState};

    use super::{McpFocus, McpHit, McpServerSnapshot, McpUiState, Text, state_icon, text};

    fn assert_click_region(ui: &McpUiState, area: ratatui::layout::Rect, expected: McpHit) {
        let found = (0..area.height)
            .any(|row| (0..area.width).any(|column| ui.clicked(column, row) == Some(expected)));
        assert!(found, "missing click region for {expected:?}");
    }

    #[test]
    fn connecting_indicator_animates_without_changing_semantics() {
        assert_ne!(
            state_icon(McpConnectionState::Connecting, 0),
            state_icon(McpConnectionState::Connecting, 1)
        );
        assert_eq!(
            state_icon(McpConnectionState::Connected, 0),
            state_icon(McpConnectionState::Connected, 99)
        );
    }

    #[test]
    fn connect_button_has_real_mouse_hit_testing() -> Result<(), Box<dyn std::error::Error>> {
        let server = McpServerSnapshot {
            name: "files".to_owned(),
            transport: "stdio",
            runtime_available: true,
            enabled: true,
            required: false,
            oauth: false,
            state: McpConnectionState::Disconnected,
            tool_count: 0,
            notice: crate::notice::UiNotice::Stopped,
        };
        let mut ui = McpUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(110, 32))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                std::slice::from_ref(&server),
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;
        let buffer = terminal.backend().buffer();
        let (column, row) = (0..buffer.area.height)
            .rev()
            .find_map(|row| {
                (0..buffer.area.width)
                    .find(|column| ui.clicked(*column, row) == Some(McpHit::Primary))
                    .map(|column| (column, row))
            })
            .ok_or_else(|| io::Error::other("Connect button has no registered click area"))?;
        assert_eq!(ui.clicked(column, row), Some(McpHit::Primary));
        let rendered_row = (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect::<String>();
        assert!(rendered_row.contains("Connect"));
        Ok(())
    }

    #[test]
    fn runtime_toggle_is_a_real_checkbox_hit_region() -> Result<(), Box<dyn std::error::Error>> {
        let server = McpServerSnapshot {
            name: "files".to_owned(),
            transport: "stdio",
            runtime_available: true,
            enabled: true,
            required: false,
            oauth: false,
            state: McpConnectionState::Connected,
            tool_count: 3,
            notice: crate::notice::UiNotice::McpToolsReady { count: 3 },
        };
        let mut ui = McpUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(110, 32))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                std::slice::from_ref(&server),
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;
        let buffer = terminal.backend().buffer();
        let (column, row) = (0..buffer.area.height)
            .find_map(|row| {
                (0..buffer.area.width)
                    .find(|column| ui.clicked(*column, row) == Some(McpHit::Toggle))
                    .map(|column| (column, row))
            })
            .ok_or_else(|| io::Error::other("MCP switch has no registered click area"))?;
        assert_eq!(ui.clicked(column, row), Some(McpHit::Toggle));
        let rendered_row = (0..buffer.area.width)
            .map(|column| buffer[(column, row)].symbol())
            .collect::<String>();
        assert!(rendered_row.contains(text(Text::EnabledThisRun)));
        Ok(())
    }

    #[test]
    fn global_disable_removes_toggle_hit_region() -> Result<(), Box<dyn std::error::Error>> {
        let server = McpServerSnapshot {
            name: "files".to_owned(),
            transport: "stdio",
            runtime_available: false,
            enabled: true,
            required: false,
            oauth: false,
            state: McpConnectionState::Disabled,
            tool_count: 0,
            notice: crate::notice::UiNotice::None,
        };
        let mut ui = McpUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(110, 32))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                std::slice::from_ref(&server),
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;
        let has_toggle = (0..terminal.backend().buffer().area.height).any(|row| {
            (0..terminal.backend().buffer().area.width)
                .any(|column| ui.clicked(column, row) == Some(McpHit::Toggle))
        });
        assert!(!has_toggle);
        Ok(())
    }

    #[test]
    fn subagent_mcp_switch_is_clickable_and_independent() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut fleet = SubagentFleetSnapshot {
            enabled: true,
            mcp_enabled: true,
            mcp_status: crate::notice::UiNotice::McpToolsReady { count: 2 },
            ..SubagentFleetSnapshot::default()
        };
        let mut ui = McpUiState::new();
        ui.open(0);
        let mut terminal = Terminal::new(TestBackend::new(110, 32))?;
        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &[], None, &fleet, true))?;
        let has_toggle = (0..32)
            .any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(McpHit::Subagents)));
        assert!(has_toggle);

        fleet.enabled = false;
        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &[], None, &fleet, true))?;
        let disabled_has_hit = (0..32)
            .any(|row| (0..110).any(|column| ui.clicked(column, row) == Some(McpHit::Subagents)));
        assert!(!disabled_has_hit);
        Ok(())
    }

    #[test]
    fn every_enabled_mcp_action_has_a_mouse_path() -> Result<(), Box<dyn std::error::Error>> {
        let server = McpServerSnapshot {
            name: "remote-tools".to_owned(),
            transport: "streamable-http",
            runtime_available: true,
            enabled: true,
            required: false,
            oauth: true,
            state: McpConnectionState::Connected,
            tool_count: 4,
            notice: crate::notice::UiNotice::McpToolsReady { count: 4 },
        };
        let fleet = SubagentFleetSnapshot {
            enabled: true,
            mcp_enabled: true,
            mcp_status: crate::notice::UiNotice::McpToolsReady { count: 2 },
            ..SubagentFleetSnapshot::default()
        };
        let mut ui = McpUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(160, 50))?;
        terminal.draw(|frame| ui.draw(frame, &[server], None, &fleet, true))?;
        let area = terminal.backend().buffer().area;
        for expected in [
            McpHit::Item(0),
            McpHit::Close,
            McpHit::Toggle,
            McpHit::Primary,
            McpHit::Secondary,
            McpHit::Subagents,
            McpHit::Add,
        ] {
            assert_click_region(&ui, area, expected);
        }
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_reach_every_mcp_control() {
        let mut ui = McpUiState::new();
        ui.open(1);
        let expected = [
            McpFocus::Primary,
            McpFocus::Secondary,
            McpFocus::Subagents,
            McpFocus::Add,
            McpFocus::Close,
            McpFocus::Toggle,
            McpFocus::Primary,
        ];
        assert_eq!(ui.focused(), Some(expected[0]));
        for focus in expected.iter().skip(1) {
            ui.next_focus();
            assert_eq!(ui.focused(), Some(*focus));
        }
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(McpFocus::Toggle));
    }

    #[test]
    fn selection_follows_the_server_across_reordering() -> Result<(), Box<dyn std::error::Error>> {
        let mut first = server("first");
        let mut second = server("second");
        first.state = McpConnectionState::Connected;
        second.state = McpConnectionState::Disconnected;
        let mut servers = vec![first, second];
        let mut ui = McpUiState::new();
        ui.open(servers.len());
        let mut terminal = Terminal::new(TestBackend::new(110, 32))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                &servers,
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;
        ui.select(1);
        terminal.draw(|frame| {
            ui.draw(
                frame,
                &servers,
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;

        servers.reverse();
        terminal.draw(|frame| {
            ui.draw(
                frame,
                &servers,
                None,
                &SubagentFleetSnapshot::default(),
                true,
            );
        })?;

        assert_eq!(servers[ui.selected_index()].name, "second");
        Ok(())
    }

    fn server(name: &str) -> McpServerSnapshot {
        McpServerSnapshot {
            name: name.to_owned(),
            transport: "stdio",
            runtime_available: true,
            enabled: true,
            required: false,
            oauth: false,
            state: McpConnectionState::Disconnected,
            tool_count: 0,
            notice: crate::notice::UiNotice::Stopped,
        }
    }
}
