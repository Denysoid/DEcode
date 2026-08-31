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

use crate::{
    agent::side_chat::has_visible_text,
    plugins::{MarketplacePlugin, PluginSnapshot},
};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(120);
const MAX_INPUT_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginFocus {
    Installed,
    Marketplace,
    Input,
    AddSource,
    InstallLocal,
    Refresh,
    Primary,
    Remove,
    RemoveSource,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginHit {
    Installed(usize),
    Marketplace(usize),
    Input,
    AddSource,
    InstallLocal,
    Refresh,
    Primary,
    Remove,
    RemoveSource,
    Close,
}

#[derive(Debug, Clone)]
pub struct PluginUiState {
    open: bool,
    dialog: DialogState<()>,
    focus: FocusManager<PluginFocus>,
    clicks: ClickRegionRegistry<PluginHit>,
    selected_installed: usize,
    selected_marketplace: usize,
    installed_ids: Vec<String>,
    marketplace_keys: Vec<(String, String, String)>,
    marketplace_active: bool,
    input: String,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl PluginUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for target in [
            PluginFocus::Installed,
            PluginFocus::Marketplace,
            PluginFocus::Input,
            PluginFocus::AddSource,
            PluginFocus::InstallLocal,
            PluginFocus::Refresh,
            PluginFocus::Primary,
            PluginFocus::Remove,
            PluginFocus::RemoveSource,
            PluginFocus::Close,
        ] {
            focus.register(target);
        }
        focus.set(PluginFocus::Installed);
        Self {
            open: false,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            selected_installed: 0,
            selected_marketplace: 0,
            installed_ids: Vec::new(),
            marketplace_keys: Vec::new(),
            marketplace_active: false,
            input: String::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, snapshot: &PluginSnapshot) {
        self.open = true;
        self.selected_installed = 0;
        self.selected_marketplace = 0;
        self.installed_ids.clear();
        self.marketplace_keys.clear();
        self.sync(snapshot);
        self.focus.set(PluginFocus::Installed);
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
    pub fn focused(&self) -> Option<PluginFocus> {
        self.focus.current().copied()
    }

    pub fn focus(&mut self, focus: PluginFocus) {
        if focus == PluginFocus::Installed {
            self.marketplace_active = false;
        } else if focus == PluginFocus::Marketplace {
            self.marketplace_active = true;
        }
        self.focus.set(focus);
    }

    pub fn focus_hit(&mut self, hit: PluginHit) {
        match hit {
            PluginHit::Installed(index) => {
                self.selected_installed = index;
                self.marketplace_active = false;
                self.focus.set(PluginFocus::Installed);
            }
            PluginHit::Marketplace(index) => {
                self.selected_marketplace = index;
                self.marketplace_active = true;
                self.focus.set(PluginFocus::Marketplace);
            }
            PluginHit::Input => self.focus.set(PluginFocus::Input),
            PluginHit::AddSource => self.focus.set(PluginFocus::AddSource),
            PluginHit::InstallLocal => self.focus.set(PluginFocus::InstallLocal),
            PluginHit::Refresh => self.focus.set(PluginFocus::Refresh),
            PluginHit::Primary => self.focus.set(PluginFocus::Primary),
            PluginHit::Remove => self.focus.set(PluginFocus::Remove),
            PluginHit::RemoveSource => self.focus.set(PluginFocus::RemoveSource),
            PluginHit::Close => self.focus.set(PluginFocus::Close),
        }
    }

    pub fn sync(&mut self, snapshot: &PluginSnapshot) {
        let selected_installed_id = self.installed_ids.get(self.selected_installed).cloned();
        let installed_ids = snapshot
            .plugins
            .iter()
            .map(|plugin| plugin.id.clone())
            .collect::<Vec<_>>();
        self.selected_installed = selected_installed_id
            .and_then(|id| installed_ids.iter().position(|candidate| candidate == &id))
            .unwrap_or_else(|| {
                self.selected_installed
                    .min(installed_ids.len().saturating_sub(1))
            });

        let selected_marketplace_key = self
            .marketplace_keys
            .get(self.selected_marketplace)
            .cloned();
        let marketplace_keys = marketplace_entries(snapshot)
            .into_iter()
            .map(|(source, plugin)| {
                (
                    source.to_owned(),
                    plugin.id.clone(),
                    plugin.version.to_string(),
                )
            })
            .collect::<Vec<_>>();
        self.selected_marketplace = selected_marketplace_key
            .and_then(|key| {
                marketplace_keys
                    .iter()
                    .position(|candidate| candidate == &key)
            })
            .unwrap_or_else(|| {
                self.selected_marketplace
                    .min(marketplace_keys.len().saturating_sub(1))
            });
        self.installed_ids = installed_ids;
        self.marketplace_keys = marketplace_keys;
        self.clicks.clear();
    }

    pub fn move_selection(&mut self, snapshot: &PluginSnapshot, forward: bool) {
        match self.focused() {
            Some(PluginFocus::Marketplace) => {
                self.marketplace_active = true;
                let total = marketplace_count(snapshot);
                self.selected_marketplace = move_index(self.selected_marketplace, total, forward);
            }
            _ => {
                self.marketplace_active = false;
                self.selected_installed =
                    move_index(self.selected_installed, snapshot.plugins.len(), forward);
                self.focus.set(PluginFocus::Installed);
            }
        }
    }

    #[must_use]
    pub const fn selected_installed(&self) -> usize {
        self.selected_installed
    }

    #[must_use]
    pub const fn selected_marketplace(&self) -> usize {
        self.selected_marketplace
    }

    #[must_use]
    pub const fn marketplace_active(&self) -> bool {
        self.marketplace_active
    }

    #[must_use]
    pub fn input(&self) -> &str {
        &self.input
    }

    #[must_use]
    pub fn input_has_visible_text(&self) -> bool {
        has_visible_text(&self.input)
    }

    pub fn clear_input(&mut self) {
        self.input.clear();
    }

    pub fn push_input(&mut self, character: char) {
        if !character.is_control()
            && self.input.len().saturating_add(character.len_utf8()) <= MAX_INPUT_BYTES
        {
            self.input.push(character);
        }
    }

    pub fn pop_input(&mut self) {
        if let Some((index, _)) = self.input.grapheme_indices(true).next_back() {
            self.input.truncate(index);
        }
    }

    pub fn set_input(&mut self, value: &str) {
        self.input.clear();
        self.push_input_text(value);
    }

    pub fn push_input_text(&mut self, value: &str) {
        for character in value.chars() {
            if character.is_control() {
                continue;
            }
            if self.input.len().saturating_add(character.len_utf8()) > MAX_INPUT_BYTES {
                break;
            }
            self.input.push(character);
        }
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PluginHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &PluginSnapshot, editable: bool) {
        if !self.open {
            return;
        }
        self.sync(snapshot);
        let focused = self.focused();
        let installed = self.selected_installed;
        let marketplace = self.selected_marketplace;
        let marketplace_active = self.marketplace_active;
        let animation = self.animation_frame;
        let input = self.input.clone();
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::PluginManager))
            .width_percent(92)
            .height_percent(90)
            .min_size(88, 30)
            .max_size(172, 58)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightMagenta)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                snapshot,
                focused,
                installed,
                marketplace,
                marketplace_active,
                animation,
                &input,
                editable,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for PluginUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &PluginSnapshot,
    focused: Option<PluginFocus>,
    selected_installed: usize,
    selected_marketplace: usize,
    marketplace_active: bool,
    animation: usize,
    input: &str,
    editable: bool,
    clicks: &mut ClickRegionRegistry<PluginHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(8),
        Constraint::Length(7),
        Constraint::Length(3),
        Constraint::Length(3),
    ])
    .split(area);
    let enabled = snapshot
        .plugins
        .iter()
        .filter(|plugin| plugin.enabled)
        .count();
    let updates = snapshot
        .plugins
        .iter()
        .filter(|plugin| plugin.update.is_some())
        .count();
    let pulse = ["◐", "◓", "◑", "◒"][animation % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {enabled}/{} {} · {updates} {} · {} {}",
                    snapshot.plugins.len(),
                    text(Text::EnabledLabel),
                    text(Text::UpdatesLabel),
                    text(Text::Revision),
                    snapshot.revision
                ),
                Style::default()
                    .fg(Color::LightMagenta)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::PluginPackageHelp)),
        ]),
        rows[0],
    );

    let columns =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(rows[1]);
    draw_installed(
        frame,
        columns[0],
        snapshot,
        focused,
        selected_installed,
        editable,
        clicks,
    );
    draw_marketplace(
        frame,
        columns[1],
        snapshot,
        focused,
        selected_marketplace,
        clicks,
    );

    let details = snapshot.plugins.get(selected_installed).map_or_else(
        || vec![Line::from(text(Text::NoInstalledPlugins))],
        |plugin| {
            let components = if plugin.components.is_empty() {
                text(Text::NoComponents).to_owned()
            } else {
                plugin.components.join(" · ")
            };
            vec![
                Line::from(vec![
                    Span::styled(
                        text(Text::SelectedPrefix),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        truncate_for_display(&sanitize_for_display(&plugin.name), 160),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(" {} · {}", plugin.version, plugin.id)),
                ]),
                Line::from(format!(
                    "{}: {} · {components}",
                    text(Text::PublisherLabel),
                    sanitize_for_display(&plugin.publisher)
                )),
                Line::from(truncate_for_display(
                    &sanitize_for_display(&plugin.description),
                    2_000,
                )),
                Line::from(if plugin.privileged {
                    Span::styled(
                        text(Text::PrivilegedPluginWarning),
                        Style::default().fg(Color::Yellow),
                    )
                } else {
                    Span::styled(
                        text(Text::NoExecutableComponents),
                        Style::default().fg(Color::Green),
                    )
                }),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(details)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(text(Text::DetailTitle)),
            )
            .wrap(Wrap { trim: false }),
        rows[2],
    );

    let input_columns = Layout::horizontal([
        Constraint::Min(30),
        Constraint::Length(18),
        Constraint::Length(18),
    ])
    .split(rows[3]);
    let input_area = input_columns[0];
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(text(Text::MarketplaceSourceInput))
        .border_style(if focused == Some(PluginFocus::Input) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    frame.render_widget(
        Paragraph::new(truncate_for_display(&sanitize_for_display(input), 4_096))
            .block(input_block),
        input_area,
    );
    clicks.register(input_area, PluginHit::Input);
    let has_input = has_visible_text(input);
    render_button(
        frame,
        input_columns[1],
        text(Text::AddSource),
        PluginHit::AddSource,
        ButtonStyle::primary(),
        PluginControlState::new(
            focused == Some(PluginFocus::AddSource),
            editable && has_input,
        ),
        clicks,
    );
    render_button(
        frame,
        input_columns[2],
        text(Text::InstallLocal),
        PluginHit::InstallLocal,
        ButtonStyle::primary(),
        PluginControlState::new(
            focused == Some(PluginFocus::InstallLocal),
            editable && has_input,
        ),
        clicks,
    );

    let buttons = Layout::horizontal([
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Length(18),
        Constraint::Fill(1),
        Constraint::Length(16),
    ])
    .split(rows[4]);
    let primary = primary_label(snapshot, marketplace_active, selected_installed);
    render_button(
        frame,
        buttons[0],
        text(Text::RefreshLabel),
        PluginHit::Refresh,
        ButtonStyle::default(),
        PluginControlState::new(focused == Some(PluginFocus::Refresh), editable),
        clicks,
    );
    render_button(
        frame,
        buttons[1],
        primary,
        PluginHit::Primary,
        ButtonStyle::primary(),
        PluginControlState::new(
            focused == Some(PluginFocus::Primary),
            editable
                && if marketplace_active {
                    marketplace_entry(snapshot, selected_marketplace).is_some()
                } else {
                    snapshot.plugins.get(selected_installed).is_some()
                },
        ),
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::RemovePlugin),
        PluginHit::Remove,
        ButtonStyle::danger(),
        PluginControlState::new(
            focused == Some(PluginFocus::Remove),
            editable && snapshot.plugins.get(selected_installed).is_some(),
        ),
        clicks,
    );
    render_button(
        frame,
        buttons[3],
        text(Text::RemoveSource),
        PluginHit::RemoveSource,
        ButtonStyle::danger(),
        PluginControlState::new(
            focused == Some(PluginFocus::RemoveSource),
            editable && marketplace_source(snapshot, selected_marketplace).is_some(),
        ),
        clicks,
    );
    render_button(
        frame,
        buttons[5],
        text(Text::CloseEsc),
        PluginHit::Close,
        ButtonStyle::default(),
        PluginControlState::new(focused == Some(PluginFocus::Close), true),
        clicks,
    );
}

fn draw_installed(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &PluginSnapshot,
    focused: Option<PluginFocus>,
    selected: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<PluginHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(text(Text::InstalledToggleTitle))
        .border_style(if focused == Some(PluginFocus::Installed) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let viewport = usize::from(inner.height);
    let start = selected
        .saturating_sub(viewport.saturating_sub(1))
        .min(snapshot.plugins.len().saturating_sub(viewport));
    for (row_index, (index, plugin)) in snapshot
        .plugins
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .enumerate()
    {
        let row = Rect::new(
            inner.x,
            inner.y.saturating_add(row_index as u16),
            inner.width,
            1,
        );
        let mut state = CheckBoxState::new(plugin.enabled);
        state.set_focused(index == selected && focused == Some(PluginFocus::Installed));
        state.set_enabled(editable);
        let update = plugin
            .update
            .as_ref()
            .map_or(String::new(), |version| format!(" · ↑{version}"));
        let label = format!(
            "{} {}{update}",
            truncate_for_display(&sanitize_for_display(&plugin.name), 80),
            plugin.version
        );
        let region = CheckBox::new(&label, &state)
            .style(
                CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                    .checked_fg(Color::Green)
                    .focused_fg(Color::LightCyan),
            )
            .render_stateful(row, frame.buffer_mut());
        if editable {
            clicks.register(region.area, PluginHit::Installed(index));
        }
    }
}

fn draw_marketplace(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &PluginSnapshot,
    focused: Option<PluginFocus>,
    selected: usize,
    clicks: &mut ClickRegionRegistry<PluginHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(text(Text::MarketplaceInstallTitle))
        .border_style(if focused == Some(PluginFocus::Marketplace) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let entries = marketplace_entries(snapshot);
    let viewport = usize::from(inner.height);
    let start = selected
        .saturating_sub(viewport.saturating_sub(1))
        .min(entries.len().saturating_sub(viewport));
    let visible_entries = entries
        .into_iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .collect::<Vec<_>>();
    for (row_index, (index, (source, plugin))) in visible_entries.iter().enumerate() {
        let row = Rect::new(
            inner.x,
            inner.y.saturating_add(row_index as u16),
            inner.width,
            1,
        );
        let selected_style = if *index == selected && focused == Some(PluginFocus::Marketplace) {
            Style::default().fg(Color::Black).bg(Color::LightCyan)
        } else {
            Style::default()
        };
        frame.render_widget(
            Paragraph::new(format!(
                " {} {} · {}",
                truncate_for_display(&sanitize_for_display(&plugin.name), 72),
                plugin.version,
                truncate_for_display(&sanitize_for_display(source), 48)
            ))
            .style(selected_style),
            row,
        );
        clicks.register(row, PluginHit::Marketplace(*index));
    }
    for (error_index, marketplace) in snapshot
        .marketplaces
        .iter()
        .filter(|marketplace| marketplace.error.is_some())
        .take(viewport.saturating_sub(visible_entries.len()))
        .enumerate()
    {
        let Some(error) = &marketplace.error else {
            continue;
        };
        let row = Rect::new(
            inner.x,
            inner
                .y
                .saturating_add((visible_entries.len() + error_index) as u16),
            inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(format!(
                " ⚠ {}: {}",
                sanitize_for_display(&marketplace.source),
                truncate_for_display(&sanitize_for_display(error), 120)
            ))
            .style(Style::default().fg(Color::Yellow)),
            row,
        );
    }
}

fn primary_label(
    snapshot: &PluginSnapshot,
    marketplace_active: bool,
    selected: usize,
) -> &'static str {
    if marketplace_active {
        text(Text::InstallSelected)
    } else if snapshot
        .plugins
        .get(selected)
        .is_some_and(|plugin| plugin.update.is_some())
    {
        text(Text::UpdateSelected)
    } else {
        text(Text::ToggleSelected)
    }
}

#[derive(Clone, Copy)]
struct PluginControlState {
    focused: bool,
    enabled: bool,
}

impl PluginControlState {
    const fn new(focused: bool, enabled: bool) -> Self {
        Self { focused, enabled }
    }
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: PluginHit,
    style: ButtonStyle,
    control: PluginControlState,
    clicks: &mut ClickRegionRegistry<PluginHit>,
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

#[must_use]
pub fn marketplace_entry(
    snapshot: &PluginSnapshot,
    index: usize,
) -> Option<(&str, &MarketplacePlugin)> {
    marketplace_entries(snapshot).into_iter().nth(index)
}

#[must_use]
pub fn marketplace_source(snapshot: &PluginSnapshot, index: usize) -> Option<&str> {
    marketplace_entry(snapshot, index)
        .map(|(source, _)| source)
        .or_else(|| {
            snapshot
                .marketplaces
                .first()
                .map(|source| source.source.as_str())
        })
}

fn marketplace_entries(snapshot: &PluginSnapshot) -> Vec<(&str, &MarketplacePlugin)> {
    snapshot
        .marketplaces
        .iter()
        .flat_map(|marketplace| {
            marketplace
                .plugins
                .iter()
                .map(move |plugin| (marketplace.source.as_str(), plugin))
        })
        .collect()
}

fn marketplace_count(snapshot: &PluginSnapshot) -> usize {
    snapshot
        .marketplaces
        .iter()
        .map(|marketplace| marketplace.plugins.len())
        .sum()
}

fn move_index(current: usize, total: usize, forward: bool) -> usize {
    if total == 0 {
        0
    } else if forward {
        current.saturating_add(1).min(total - 1)
    } else {
        current.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use ratatui::{Terminal, backend::TestBackend};
    use semver::Version;

    use super::{PluginFocus, PluginHit, PluginUiState};
    use crate::plugins::{MarketplacePlugin, MarketplaceSummary, PluginSnapshot, PluginSummary};

    fn snapshot() -> PluginSnapshot {
        PluginSnapshot {
            revision: 4,
            root: PathBuf::from("plugins"),
            plugins: Arc::from([PluginSummary {
                id: "dev.example.review".to_owned(),
                name: "Review pack".to_owned(),
                version: Version::new(1, 0, 0),
                description: "Review helpers".to_owned(),
                publisher: "denysoid".to_owned(),
                enabled: true,
                source: "local".to_owned(),
                components: Arc::from(["skills ×1".to_owned()]),
                privileged: false,
                update: Some(Version::new(1, 1, 0)),
            }]),
            marketplaces: Arc::from([MarketplaceSummary {
                source: "https://example.test/index.json".to_owned(),
                name: "Example".to_owned(),
                plugins: Arc::from([]),
                error: None,
            }]),
            diagnostics: Arc::from([]),
        }
    }

    #[test]
    fn plugin_manager_has_mouse_regions_and_tab_fallback() -> Result<(), Box<dyn std::error::Error>>
    {
        let snapshot = snapshot();
        let mut state = PluginUiState::new();
        state.open(&snapshot);
        let backend = TestBackend::new(130, 44);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Plugin Manager"));
        assert!(rendered.contains("Review pack"));
        let mut installed = false;
        let mut input = false;
        let mut remove_source = false;
        let mut close = false;
        for row in 0..44 {
            for column in 0..130 {
                match state.clicked(column, row) {
                    Some(PluginHit::Installed(0)) => installed = true,
                    Some(PluginHit::Input) => input = true,
                    Some(PluginHit::RemoveSource) => remove_source = true,
                    Some(PluginHit::Close) => close = true,
                    _ => {}
                }
            }
        }
        assert!(installed && input && remove_source && close);
        assert_eq!(state.focused(), Some(PluginFocus::Installed));
        state.next_focus();
        assert_eq!(state.focused(), Some(PluginFocus::Marketplace));
        Ok(())
    }

    fn plugin(id: &str) -> PluginSummary {
        PluginSummary {
            id: id.to_owned(),
            name: id.to_owned(),
            version: Version::new(1, 0, 0),
            description: String::new(),
            publisher: String::new(),
            enabled: true,
            source: "local".to_owned(),
            components: Arc::from([]),
            privileged: false,
            update: None,
        }
    }

    fn marketplace_plugin(id: &str) -> MarketplacePlugin {
        MarketplacePlugin {
            id: id.to_owned(),
            name: id.to_owned(),
            version: Version::new(1, 0, 0),
            description: String::new(),
            package_url: format!("https://example.test/{id}.zip"),
            sha256: "0".repeat(64),
        }
    }

    #[test]
    fn input_editor_is_byte_bounded_and_grapheme_aware() {
        let mut ui = PluginUiState::new();
        ui.set_input(&format!("{}é", "a".repeat(4_095)));
        assert_eq!(ui.input().len(), 4_095);

        ui.clear_input();
        ui.push_input('e');
        ui.push_input('\u{301}');
        ui.pop_input();
        assert!(ui.input().is_empty());
    }

    #[test]
    fn selections_follow_plugin_identity_across_reordering() {
        let initial = PluginSnapshot {
            plugins: Arc::from([plugin("a"), plugin("b")]),
            marketplaces: Arc::from([MarketplaceSummary {
                source: "https://example.test/index.json".to_owned(),
                name: "Example".to_owned(),
                plugins: Arc::from([marketplace_plugin("a"), marketplace_plugin("b")]),
                error: None,
            }]),
            ..PluginSnapshot::default()
        };
        let updated = PluginSnapshot {
            revision: 2,
            plugins: Arc::from([plugin("new"), plugin("a"), plugin("b")]),
            marketplaces: Arc::from([MarketplaceSummary {
                source: "https://example.test/index.json".to_owned(),
                name: "Example".to_owned(),
                plugins: Arc::from([
                    marketplace_plugin("new"),
                    marketplace_plugin("a"),
                    marketplace_plugin("b"),
                ]),
                error: None,
            }]),
            ..PluginSnapshot::default()
        };
        let mut ui = PluginUiState::new();
        ui.open(&initial);
        ui.focus_hit(PluginHit::Installed(1));
        ui.focus_hit(PluginHit::Marketplace(1));

        ui.sync(&updated);

        assert_eq!(updated.plugins[ui.selected_installed()].id, "b");
        assert_eq!(
            super::marketplace_entry(&updated, ui.selected_marketplace())
                .map(|(_, plugin)| plugin.id.as_str()),
            Some("b")
        );
    }

    #[test]
    fn selected_installed_plugin_is_scrolled_into_view() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = PluginSnapshot {
            plugins: (0..40)
                .map(|index| plugin(&format!("plugin-{index}")))
                .collect::<Vec<_>>()
                .into(),
            ..PluginSnapshot::default()
        };
        let mut ui = PluginUiState::new();
        ui.open(&snapshot);
        for _ in 0..39 {
            ui.move_selection(&snapshot, true);
        }
        let mut terminal = Terminal::new(TestBackend::new(130, 44))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;

        assert!((0..44).any(|row| {
            (0..130).any(|column| ui.clicked(column, row) == Some(PluginHit::Installed(39)))
        }));
        Ok(())
    }

    #[test]
    fn marketplace_errors_do_not_overwrite_plugin_rows() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = PluginSnapshot {
            marketplaces: Arc::from([
                MarketplaceSummary {
                    source: "https://good.test/index.json".to_owned(),
                    name: "Good".to_owned(),
                    plugins: Arc::from([marketplace_plugin("visible-package")]),
                    error: None,
                },
                MarketplaceSummary {
                    source: "https://bad.test/index.json".to_owned(),
                    name: "Bad".to_owned(),
                    plugins: Arc::from([]),
                    error: Some("network failed".to_owned()),
                },
            ]),
            ..PluginSnapshot::default()
        };
        let mut ui = PluginUiState::new();
        ui.open(&snapshot);
        let mut terminal = Terminal::new(TestBackend::new(130, 44))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;
        let rendered = terminal.backend().to_string();

        assert!(rendered.contains("visible-package"));
        assert!(rendered.contains("network failed"));
        Ok(())
    }
}
