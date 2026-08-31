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

use crate::agent::{InstructionOrigin, InstructionSetSnapshot, InstructionSourceSnapshot};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(160);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionsFocus {
    Global,
    Sources,
    Reload,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstructionsHit {
    Global,
    Source(usize),
    Reload,
    Close,
}

#[derive(Debug, Clone)]
pub struct InstructionsUiState {
    open: bool,
    dialog: DialogState<()>,
    focus: FocusManager<InstructionsFocus>,
    clicks: ClickRegionRegistry<InstructionsHit>,
    selected: usize,
    source_ids: Vec<String>,
    selected_id: Option<String>,
    snapshot_revision: Option<u64>,
    scroll: usize,
    visible_rows: usize,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl InstructionsUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            InstructionsFocus::Global,
            InstructionsFocus::Sources,
            InstructionsFocus::Reload,
            InstructionsFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(InstructionsFocus::Sources);
        Self {
            open: false,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            selected: 0,
            source_ids: Vec::new(),
            selected_id: None,
            snapshot_revision: None,
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

    pub fn open(&mut self, total: usize) {
        self.open = true;
        self.selected = self.selected.min(total.saturating_sub(1));
        if total == 0 {
            self.selected_id = None;
        }
        self.ensure_visible(total);
        self.focus.set(InstructionsFocus::Sources);
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
    pub fn focused(&self) -> Option<InstructionsFocus> {
        self.focus.current().copied()
    }

    pub fn focus_hit(&mut self, hit: InstructionsHit) {
        match hit {
            InstructionsHit::Global => self.focus.set(InstructionsFocus::Global),
            InstructionsHit::Source(index) => {
                self.selected = index;
                self.update_selected_id();
                self.focus.set(InstructionsFocus::Sources);
            }
            InstructionsHit::Reload => self.focus.set(InstructionsFocus::Reload),
            InstructionsHit::Close => self.focus.set(InstructionsFocus::Close),
        }
    }

    pub fn sync(&mut self, snapshot: &InstructionSetSnapshot) {
        if self.snapshot_revision != Some(snapshot.revision) {
            self.clicks.clear();
            self.snapshot_revision = Some(snapshot.revision);
        }
        if let Some(id) = self.selected_id.as_deref()
            && let Some(index) = snapshot.sources.iter().position(|source| source.id == id)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(snapshot.sources.len().saturating_sub(1));
        self.source_ids = snapshot
            .sources
            .iter()
            .map(|source| source.id.clone())
            .collect();
        self.update_selected_id();
        self.ensure_visible(snapshot.sources.len());
    }

    pub fn next_source(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = (self.selected + 1).min(total - 1);
        self.update_selected_id();
        self.ensure_visible(total);
    }

    pub fn previous_source(&mut self, total: usize) {
        if total == 0 {
            return;
        }
        self.selected = self.selected.saturating_sub(1);
        self.update_selected_id();
        self.ensure_visible(total);
    }

    pub fn page_sources(&mut self, total: usize, forward: bool) {
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
        self.update_selected_id();
        self.ensure_visible(total);
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<InstructionsHit> {
        self.clicks.handle_click(column, row).copied()
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

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        snapshot: &InstructionSetSnapshot,
        editable: bool,
    ) {
        if !self.open {
            return;
        }
        self.sync(snapshot);
        let focused = self.focus.current().copied();
        let selected = self.selected;
        let scroll = self.scroll;
        let animation_frame = self.animation_frame;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::RepositoryInstructions))
            .width_percent(84)
            .height_percent(82)
            .min_size(72, 24)
            .max_size(146, 52)
            .border_color(Color::Blue)
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
                focused,
                selected,
                scroll,
                animation_frame,
                editable,
                clicks,
            );
        });
        popup.render(frame);
        self.visible_rows = visible_rows.max(1);
        self.ensure_visible(snapshot.sources.len());
    }

    fn update_selected_id(&mut self) {
        self.selected_id = self.source_ids.get(self.selected).cloned();
    }
}

impl Default for InstructionsUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &InstructionSetSnapshot,
    focused: Option<InstructionsFocus>,
    selected: usize,
    scroll: usize,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<InstructionsHit>,
) -> usize {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(7),
        Constraint::Length(4),
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["◐", "◓", "◑", "◒"][animation_frame % 4];
    let active_sources = snapshot
        .sources
        .iter()
        .filter(|source| source.locked || (snapshot.project_enabled && source.enabled))
        .count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {active_sources}/{} {} · {} {} · {} {}",
                    snapshot.sources.len(),
                    text(Text::SourcesActive),
                    snapshot.active_project_bytes,
                    text(Text::ProjectBytes),
                    text(Text::Revision),
                    snapshot.revision
                ),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::NestedScopesHelp)),
        ]),
        rows[0],
    );

    let mut global = CheckBoxState::new(snapshot.project_enabled);
    global.set_enabled(editable);
    global.set_focused(focused == Some(InstructionsFocus::Global));
    let global_region = CheckBox::new(text(Text::EnableRepositoryGuidance), &global)
        .style(
            CheckBoxStyle::custom(text(Text::EnabledLabel), text(Text::DisabledLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(rows[1], frame.buffer_mut());
    if editable {
        clicks.register(global_region.area, InstructionsHit::Global);
    }

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(InstructionsFocus::Sources) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(format!(" {} ", text(Text::ActiveHierarchyHelp)));
    let inner = list_block.inner(rows[2]);
    frame.render_widget(list_block, rows[2]);
    let visible = usize::from(inner.height);
    for (line_index, (index, source)) in snapshot
        .sources
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible)
        .enumerate()
    {
        let row = Rect::new(
            inner.x,
            inner
                .y
                .saturating_add(u16::try_from(line_index).unwrap_or(u16::MAX)),
            inner.width,
            1,
        );
        render_source(
            frame,
            row,
            source,
            index == selected && focused == Some(InstructionsFocus::Sources),
            !source.locked && snapshot.project_enabled && editable,
            index,
            clicks,
        );
    }

    let details = snapshot.sources.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoInstructionSources))],
        |source| {
            vec![
                Line::from(vec![
                    Span::styled(
                        format!("{}: ", text(Text::ScopeLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(truncate_for_display(
                        &sanitize_for_display(&source.scope),
                        512,
                    )),
                    Span::styled(
                        format!(" · {}: ", text(Text::OriginLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(origin_label(source.origin)),
                ]),
                Line::from(format!(
                    "{} {} · {} {} · ID {}",
                    source.bytes,
                    text(Text::Bytes),
                    source.include_count,
                    text(Text::ResolvedIncludes),
                    truncate_for_display(&sanitize_for_display(&source.id), 1_024)
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(details)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::SelectedSource))),
            )
            .wrap(Wrap { trim: false }),
        rows[3],
    );

    let warning_lines = if snapshot.warnings.is_empty() {
        vec![Line::from(Span::styled(
            format!("✓ {}", text(Text::NoInstructionWarnings)),
            Style::default().fg(Color::Green),
        ))]
    } else {
        snapshot
            .warnings
            .iter()
            .take(2)
            .map(|warning| {
                Line::from(Span::styled(
                    format!(
                        "⚠ {}",
                        truncate_for_display(&sanitize_for_display(warning), 1_000)
                    ),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(warning_lines)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · {} ",
                text(Text::DiagnosticsLabel),
                snapshot.warnings.len()
            )))
            .wrap(Wrap { trim: false }),
        rows[4],
    );

    let buttons = Layout::horizontal([
        Constraint::Length(19),
        Constraint::Length(1),
        Constraint::Length(19),
        Constraint::Fill(1),
    ])
    .split(rows[5]);
    render_button(
        frame,
        buttons[0],
        text(Text::ReloadFiles),
        InstructionsHit::Reload,
        focused == Some(InstructionsFocus::Reload),
        editable,
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::CloseEsc),
        InstructionsHit::Close,
        focused == Some(InstructionsFocus::Close),
        true,
        clicks,
    );
    visible.max(1)
}

fn render_source(
    frame: &mut Frame<'_>,
    area: Rect,
    source: &InstructionSourceSnapshot,
    focused: bool,
    enabled: bool,
    index: usize,
    clicks: &mut ClickRegionRegistry<InstructionsHit>,
) {
    let mut state = CheckBoxState::new(source.enabled);
    state.set_enabled(enabled);
    state.set_focused(focused);
    let label = format!(
        "{} {} · {} · {} B{}",
        if source.locked { "🔒" } else { "" },
        truncate_for_display(&sanitize_for_display(&source.display_path), 140),
        truncate_for_display(&sanitize_for_display(&source.scope), 80),
        source.origin,
        source.bytes
    );
    let region = CheckBox::new(&label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::EnabledLabel), text(Text::DisabledLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    if !source.locked {
        clicks.register(region.area, InstructionsHit::Source(index));
    } else {
        clicks.register(area, InstructionsHit::Source(index));
    }
}

fn origin_label(origin: InstructionOrigin) -> &'static str {
    match origin {
        InstructionOrigin::System => text(Text::TrustedSystemOrigin),
        InstructionOrigin::Project => text(Text::RepositoryOrigin),
    }
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: InstructionsHit,
    focused: bool,
    enabled: bool,
    clicks: &mut ClickRegionRegistry<InstructionsHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(if matches!(hit, InstructionsHit::Reload) {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        })
        .render_stateful(area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, hit);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

    use super::InstructionsUiState;
    use crate::agent::{InstructionOrigin, InstructionSetSnapshot, InstructionSourceSnapshot};

    #[test]
    fn dialog_renders_scope_status_and_diagnostics() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = InstructionSetSnapshot {
            revision: 7,
            project_enabled: true,
            active_project_bytes: 42,
            sources: Arc::from([
                InstructionSourceSnapshot {
                    id: "system".to_owned(),
                    display_path: "trusted.md".to_owned(),
                    scope: "all requests".to_owned(),
                    origin: InstructionOrigin::System,
                    bytes: 12,
                    include_count: 0,
                    enabled: true,
                    locked: true,
                },
                InstructionSourceSnapshot {
                    id: "project:frontend/AGENTS.md".to_owned(),
                    display_path: "frontend/AGENTS.md".to_owned(),
                    scope: "frontend".to_owned(),
                    origin: InstructionOrigin::Project,
                    bytes: 42,
                    include_count: 1,
                    enabled: true,
                    locked: false,
                },
            ]),
            warnings: Arc::from(["cycle ignored".to_owned()]),
        };
        let mut state = InstructionsUiState::new();
        state.open(snapshot.sources.len());
        let backend = TestBackend::new(120, 38);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Repository instructions"));
        assert!(rendered.contains("frontend/AGENTS.md"));
        assert!(rendered.contains("cycle ignored"));
        Ok(())
    }

    #[test]
    fn selection_follows_the_source_across_reordering() -> Result<(), Box<dyn std::error::Error>> {
        let mut snapshot = instruction_snapshot(["first", "second"]);
        let mut state = InstructionsUiState::new();
        state.open(snapshot.sources.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 38))?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;
        state.focus_hit(super::InstructionsHit::Source(1));

        snapshot.revision = 2;
        snapshot.sources = Arc::from([snapshot.sources[1].clone(), snapshot.sources[0].clone()]);
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;

        assert_eq!(snapshot.sources[state.selected()].id, "second");
        Ok(())
    }

    fn instruction_snapshot(ids: [&str; 2]) -> InstructionSetSnapshot {
        InstructionSetSnapshot {
            revision: 1,
            project_enabled: true,
            sources: ids
                .map(|id| InstructionSourceSnapshot {
                    id: id.to_owned(),
                    display_path: format!("{id}.md"),
                    scope: "workspace".to_owned(),
                    origin: InstructionOrigin::Project,
                    bytes: 0,
                    include_count: 0,
                    enabled: true,
                    locked: false,
                })
                .into(),
            ..InstructionSetSnapshot::default()
        }
    }
}
