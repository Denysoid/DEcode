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

use crate::agent::automation::{AutomationSnapshot, AutomationSource};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomationPane {
    Commands,
    Hooks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutomationFocus {
    Commands,
    Hooks,
    Items,
    Primary,
    Reload,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AutomationHit {
    Commands,
    Hooks,
    Item(usize),
    ToggleHook(usize),
    Primary,
    Reload,
    Close,
}

#[derive(Debug, Clone)]
pub struct AutomationUiState {
    open: bool,
    pane: AutomationPane,
    dialog: DialogState<()>,
    focus: FocusManager<AutomationFocus>,
    clicks: ClickRegionRegistry<AutomationHit>,
    selected: usize,
    scroll: usize,
    visible_rows: usize,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl AutomationUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            AutomationFocus::Commands,
            AutomationFocus::Hooks,
            AutomationFocus::Items,
            AutomationFocus::Primary,
            AutomationFocus::Reload,
            AutomationFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(AutomationFocus::Items);
        Self {
            open: false,
            pane: AutomationPane::Commands,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            selected: 0,
            scroll: 0,
            visible_rows: 1,
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    #[must_use]
    pub const fn pane(&self) -> AutomationPane {
        self.pane
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn focused(&self) -> Option<AutomationFocus> {
        self.focus.current().copied()
    }

    pub fn open(&mut self, snapshot: &AutomationSnapshot) {
        self.open = true;
        self.pane = if snapshot.commands.is_empty() && !snapshot.hooks.is_empty() {
            AutomationPane::Hooks
        } else {
            AutomationPane::Commands
        };
        self.selected = 0;
        self.scroll = 0;
        self.focus.set(AutomationFocus::Items);
        self.ensure_visible(item_count(snapshot, self.pane));
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

    pub fn set_pane(&mut self, pane: AutomationPane, total: usize) {
        self.pane = pane;
        self.selected = 0;
        self.scroll = 0;
        self.ensure_visible(total);
        self.focus.set(match pane {
            AutomationPane::Commands => AutomationFocus::Commands,
            AutomationPane::Hooks => AutomationFocus::Hooks,
        });
    }

    pub fn focus_items(&mut self) {
        self.focus.set(AutomationFocus::Items);
    }

    pub fn focus_hit(&mut self, hit: AutomationHit) {
        match hit {
            AutomationHit::Commands => self.focus.set(AutomationFocus::Commands),
            AutomationHit::Hooks => self.focus.set(AutomationFocus::Hooks),
            AutomationHit::Item(index) | AutomationHit::ToggleHook(index) => {
                self.selected = index;
                self.focus.set(AutomationFocus::Items);
            }
            AutomationHit::Primary => self.focus.set(AutomationFocus::Primary),
            AutomationHit::Reload => self.focus.set(AutomationFocus::Reload),
            AutomationHit::Close => self.focus.set(AutomationFocus::Close),
        }
    }

    pub fn next_item(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = self.selected.saturating_add(1).min(total - 1);
        self.ensure_visible(total);
    }

    pub fn previous_item(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.ensure_visible(total);
    }

    pub fn first_item(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = 0;
        self.ensure_visible(total);
    }

    pub fn last_item(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = total - 1;
        self.ensure_visible(total);
    }

    pub fn page_items(&mut self, total: usize, forward: bool) {
        if total == 0 {
            return;
        }
        if forward {
            self.selected = self
                .selected
                .saturating_add(self.visible_rows.max(1))
                .min(total - 1);
        } else {
            self.selected = self.selected.saturating_sub(self.visible_rows.max(1));
        }
        self.ensure_visible(total);
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<AutomationHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn sync(&mut self, snapshot: &AutomationSnapshot) {
        self.ensure_visible(item_count(snapshot, self.pane));
    }

    fn ensure_visible(&mut self, total: usize) {
        self.selected = self.selected.min(total.saturating_sub(1));
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(self.visible_rows.max(1)) {
            self.scroll = self
                .selected
                .saturating_add(1)
                .saturating_sub(self.visible_rows.max(1));
        }
        self.scroll = self.scroll.min(total.saturating_sub(1));
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &AutomationSnapshot) {
        if !self.open {
            return;
        }
        self.ensure_visible(item_count(snapshot, self.pane));
        let pane = self.pane;
        let focused = self.focus.current().copied();
        let selected = self.selected;
        let scroll = self.scroll;
        let animation_frame = self.animation_frame;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::Automation))
            .width_percent(88)
            .height_percent(86)
            .min_size(76, 25)
            .max_size(152, 56)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut visible_rows = 1_usize;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            visible_rows = draw_content(
                frame,
                area,
                snapshot,
                pane,
                focused,
                selected,
                scroll,
                animation_frame,
                clicks,
            );
        });
        popup.render(frame);
        self.visible_rows = visible_rows.max(1);
        self.ensure_visible(item_count(snapshot, self.pane));
    }
}

impl Default for AutomationUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AutomationSnapshot,
    pane: AutomationPane,
    focused: Option<AutomationFocus>,
    selected: usize,
    scroll: usize,
    animation_frame: usize,
    clicks: &mut ClickRegionRegistry<AutomationHit>,
) -> usize {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Length(7),
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .split(area);
    draw_header(frame, rows[0], snapshot, animation_frame);
    draw_tabs(frame, rows[1], pane, focused, clicks);

    let visible_rows = draw_items(
        frame, rows[2], snapshot, pane, focused, selected, scroll, clicks,
    );
    draw_details(frame, rows[3], snapshot, pane, selected);
    draw_diagnostics(frame, rows[4], snapshot);
    draw_buttons(frame, rows[5], snapshot, pane, focused, selected, clicks);
    visible_rows
}

fn draw_header(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AutomationSnapshot,
    animation_frame: usize,
) {
    let pulse = ["·", "✦", "•", "✦"][animation_frame % 4];
    let enabled_hooks = snapshot.hooks.iter().filter(|hook| hook.enabled).count();
    let path = snapshot.user_hooks_dir.as_ref().map_or_else(
        || text(Text::Unavailable).to_owned(),
        |path| path.display().to_string(),
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {} {} · {enabled_hooks}/{} {} · {} {}",
                    snapshot.commands.len(),
                    text(Text::CommandsCountLabel),
                    snapshot.hooks.len(),
                    text(Text::HooksActiveLabel),
                    text(Text::Revision),
                    snapshot.revision
                ),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{}: {}",
                text(Text::TrustedExecutableHooks),
                truncate_for_display(&sanitize_for_display(&path), 1_024)
            )),
            Line::from(text(Text::AutomationHookSafetyHelp)),
        ])
        .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_tabs(
    frame: &mut Frame<'_>,
    area: Rect,
    pane: AutomationPane,
    focused: Option<AutomationFocus>,
    clicks: &mut ClickRegionRegistry<AutomationHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(25),
        Constraint::Length(2),
        Constraint::Length(25),
        Constraint::Fill(1),
    ])
    .split(area);
    let commands = render_button(
        frame,
        columns[1],
        text(Text::CustomCommands),
        focused == Some(AutomationFocus::Commands),
        matches!(pane, AutomationPane::Commands),
    );
    clicks.register(commands, AutomationHit::Commands);
    let hooks = render_button(
        frame,
        columns[3],
        text(Text::LifecycleHooks),
        focused == Some(AutomationFocus::Hooks),
        matches!(pane, AutomationPane::Hooks),
    );
    clicks.register(hooks, AutomationHit::Hooks);
}

#[allow(clippy::too_many_arguments)]
fn draw_items(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AutomationSnapshot,
    pane: AutomationPane,
    focused: Option<AutomationFocus>,
    selected: usize,
    scroll: usize,
    clicks: &mut ClickRegionRegistry<AutomationHit>,
) -> usize {
    let title = match pane {
        AutomationPane::Commands => text(Text::CommandsTabHelp),
        AutomationPane::Hooks => text(Text::HooksTabHelp),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(AutomationFocus::Items) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let visible = usize::from(inner.height);
    let total = item_count(snapshot, pane);
    if total == 0 {
        frame.render_widget(
            Paragraph::new(match pane {
                AutomationPane::Commands => text(Text::NoCommandToml),
                AutomationPane::Hooks => text(Text::NoTrustedHooks),
            }),
            inner,
        );
        return visible.max(1);
    }
    for (row_index, index) in (scroll..total).take(visible).enumerate() {
        let row = Rect::new(
            inner.x,
            inner
                .y
                .saturating_add(u16::try_from(row_index).unwrap_or(u16::MAX)),
            inner.width,
            1,
        );
        let is_selected = index == selected;
        match pane {
            AutomationPane::Commands => {
                if let Some(command) = snapshot.commands.get(index) {
                    let source = match command.source {
                        AutomationSource::User => text(Text::UserSource),
                        AutomationSource::Project => text(Text::ProjectSource),
                    };
                    let label = format!(
                        "/{}  {}  [{}]",
                        sanitize_for_display(&command.id),
                        sanitize_for_display(&command.name),
                        source
                    );
                    let style = selection_style(is_selected, focused);
                    frame.render_widget(
                        Paragraph::new(truncate_for_display(&label, 1_024)).style(style),
                        row,
                    );
                    clicks.register(row, AutomationHit::Item(index));
                }
            }
            AutomationPane::Hooks => {
                if let Some(hook) = snapshot.hooks.get(index) {
                    let mut state = CheckBoxState::new(hook.enabled);
                    state.set_focused(is_selected && focused == Some(AutomationFocus::Items));
                    let label = format!(
                        "{} · {}{}",
                        sanitize_for_display(&hook.name),
                        hook.event,
                        if hook.blocking {
                            text(Text::BlockingLabel)
                        } else {
                            ""
                        }
                    );
                    let label = truncate_for_display(&label, 1_024);
                    let checkbox = CheckBox::new(&label, &state).style(
                        CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                            .checked_fg(Color::Green)
                            .focused_fg(Color::LightCyan),
                    );
                    let region = checkbox.render_stateful(row, frame.buffer_mut());
                    clicks.register(region.area, AutomationHit::ToggleHook(index));
                }
            }
        }
    }
    visible.max(1)
}

fn selection_style(selected: bool, focused: Option<AutomationFocus>) -> Style {
    if selected && focused == Some(AutomationFocus::Items) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else if selected {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn draw_details(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AutomationSnapshot,
    pane: AutomationPane,
    selected: usize,
) {
    let lines = match pane {
        AutomationPane::Commands => snapshot.commands.get(selected).map_or_else(
            || vec![Line::from(text(Text::SelectCustomCommand))],
            |command| {
                let hint = if command.argument_hint.is_empty() {
                    text(Text::NoArgumentHint).to_owned()
                } else {
                    sanitize_for_display(&command.argument_hint)
                };
                vec![
                    Line::from(vec![
                        Span::styled(
                            format!("/{}", sanitize_for_display(&command.id)),
                            Style::default()
                                .fg(Color::LightCyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(" · {hint}")),
                    ]),
                    Line::from(truncate_for_display(
                        &sanitize_for_display(&command.description),
                        2_000,
                    )),
                    Line::from(format!(
                        "{}: {}",
                        text(Text::SourceLabel),
                        truncate_for_display(
                            &sanitize_for_display(&command.source_path.display().to_string()),
                            2_000
                        )
                    )),
                ]
            },
        ),
        AutomationPane::Hooks => snapshot.hooks.get(selected).map_or_else(
            || vec![Line::from(text(Text::SelectHook))],
            |hook| {
                let args = hook
                    .args
                    .iter()
                    .map(|argument| sanitize_for_display(argument))
                    .collect::<Vec<_>>()
                    .join(" ");
                let matchers = hook
                    .tool_match
                    .iter()
                    .map(|matcher| sanitize_for_display(matcher))
                    .collect::<Vec<_>>()
                    .join(", ");
                vec![
                    Line::from(truncate_for_display(
                        &sanitize_for_display(&hook.description),
                        2_000,
                    )),
                    Line::from(format!(
                        "{}: {} {}",
                        text(Text::ProgramLabel),
                        truncate_for_display(
                            &sanitize_for_display(&hook.program.display().to_string()),
                            1_000
                        ),
                        truncate_for_display(&args, 1_000)
                    )),
                    Line::from(format!(
                        "{}: {} ms · {}: {}",
                        text(Text::TimeoutLabel),
                        hook.timeout.as_millis(),
                        text(Text::McpToolsCountLabel),
                        if matchers.is_empty() {
                            "—"
                        } else {
                            &matchers
                        }
                    )),
                ]
            },
        ),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(text(Text::DetailTitle)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_diagnostics(frame: &mut Frame<'_>, area: Rect, snapshot: &AutomationSnapshot) {
    let lines = if snapshot.diagnostics.is_empty() {
        vec![Line::from(Span::styled(
            text(Text::DefinitionsLoadedClean),
            Style::default().fg(Color::Green),
        ))]
    } else {
        snapshot
            .diagnostics
            .iter()
            .take(2)
            .map(|diagnostic| {
                Line::from(Span::styled(
                    format!(
                        "⚠ {}",
                        truncate_for_display(&sanitize_for_display(diagnostic), 1_500)
                    ),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · {} ",
                text(Text::DiagnosticsLabel),
                snapshot.diagnostics.len()
            )))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &AutomationSnapshot,
    pane: AutomationPane,
    focused: Option<AutomationFocus>,
    selected: usize,
    clicks: &mut ClickRegionRegistry<AutomationHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Length(22),
        Constraint::Length(1),
        Constraint::Length(17),
        Constraint::Fill(1),
        Constraint::Length(15),
    ])
    .split(area);
    let has_item = selected < item_count(snapshot, pane);
    let primary_label = match pane {
        AutomationPane::Commands => text(Text::InsertCommand),
        AutomationPane::Hooks => text(Text::ToggleSelected),
    };
    let primary = render_enabled_button(
        frame,
        columns[0],
        primary_label,
        focused == Some(AutomationFocus::Primary),
        has_item,
        true,
    );
    if has_item {
        clicks.register(primary, AutomationHit::Primary);
    }
    let reload = render_enabled_button(
        frame,
        columns[2],
        text(Text::ReloadToml),
        focused == Some(AutomationFocus::Reload),
        true,
        false,
    );
    clicks.register(reload, AutomationHit::Reload);
    let close = render_enabled_button(
        frame,
        columns[4],
        text(Text::Close),
        focused == Some(AutomationFocus::Close),
        true,
        false,
    );
    clicks.register(close, AutomationHit::Close);
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    active: bool,
) -> Rect {
    render_enabled_button(frame, area, label, focused, true, active)
}

fn render_enabled_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    enabled: bool,
    primary: bool,
) -> Rect {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let style = if primary {
        ButtonStyle::primary()
    } else {
        ButtonStyle::default()
    };
    Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style)
        .render_stateful(area, frame.buffer_mut())
        .area
}

#[must_use]
pub fn item_count(snapshot: &AutomationSnapshot, pane: AutomationPane) -> usize {
    match pane {
        AutomationPane::Commands => snapshot.commands.len(),
        AutomationPane::Hooks => snapshot.hooks.len(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::agent::automation::{
        AutomationSnapshot, AutomationSource, CustomCommandSummary, HookEvent, HookSummary,
    };

    use super::{AutomationHit, AutomationPane, AutomationUiState};

    fn snapshot() -> AutomationSnapshot {
        AutomationSnapshot {
            revision: 3,
            user_commands_dir: Some(PathBuf::from("user/commands")),
            project_commands_dir: PathBuf::from("project/commands"),
            user_hooks_dir: Some(PathBuf::from("user/hooks")),
            commands: vec![CustomCommandSummary {
                id: "review".to_owned(),
                name: "Review".to_owned(),
                description: "Review a change".to_owned(),
                source: AutomationSource::Project,
                source_path: PathBuf::from("command.toml"),
                argument_hint: "<path>".to_owned(),
                requires_arguments: true,
            }]
            .into(),
            hooks: vec![HookSummary {
                id: "guard".to_owned(),
                name: "Guard".to_owned(),
                description: "Checks tools".to_owned(),
                source_path: PathBuf::from("hook.toml"),
                event: HookEvent::PreToolUse,
                program: PathBuf::from("C:/tools/guard.exe"),
                args: Arc::from([]),
                timeout: Duration::from_secs(1),
                blocking: true,
                enabled: true,
                tool_match: vec!["execute_command".to_owned()].into(),
            }]
            .into(),
            diagnostics: Arc::from([]),
        }
    }

    #[test]
    fn tabs_and_hook_checkbox_are_real_mouse_regions() {
        let snapshot = snapshot();
        let mut ui = AutomationUiState::new();
        ui.open(&snapshot);
        let backend = TestBackend::new(110, 38);
        let mut terminal = Terminal::new(backend).unwrap_or_else(|never| match never {});
        terminal
            .draw(|frame| {
                ui.begin_frame();
                ui.draw(frame, &snapshot);
            })
            .unwrap_or_else(|never| match never {});

        let mut found_hooks = false;
        for row in 0..38 {
            for column in 0..110 {
                if ui.clicked(column, row) == Some(AutomationHit::Hooks) {
                    found_hooks = true;
                    break;
                }
            }
        }
        assert!(found_hooks);

        ui.set_pane(AutomationPane::Hooks, snapshot.hooks.len());
        terminal
            .draw(|frame| {
                ui.begin_frame();
                ui.draw(frame, &snapshot);
            })
            .unwrap_or_else(|never| match never {});
        let mut found_toggle = false;
        for row in 0..38 {
            for column in 0..110 {
                if ui.clicked(column, row) == Some(AutomationHit::ToggleHook(0)) {
                    found_toggle = true;
                    break;
                }
            }
        }
        assert!(found_toggle);
    }

    #[test]
    fn tab_navigation_has_a_keyboard_fallback() {
        let mut ui = AutomationUiState::new();
        ui.open(&snapshot());
        let initial = ui.focused();
        ui.next_focus();
        assert_ne!(ui.focused(), initial);
        ui.previous_focus();
        assert_eq!(ui.focused(), initial);
    }
}
