use std::time::{Duration, Instant};

use super::actions::ClickRegionRegistry;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Wrap},
};
use ratatui_interact::{
    components::{
        Button, ButtonState, ButtonStyle, ButtonVariant, CheckBox, CheckBoxState, CheckBoxStyle,
        DialogConfig, DialogState, PopupDialog,
    },
    state::FocusManager,
};

use crate::agent::{SkillCatalogSnapshot, SkillSummary};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillsFocus {
    Skills,
    Reload,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillsHit {
    Skill(usize),
    Reload,
    Close,
}

#[derive(Debug, Clone)]
pub struct SkillsUiState {
    open: bool,
    dialog: DialogState<()>,
    focus: FocusManager<SkillsFocus>,
    clicks: ClickRegionRegistry<SkillsHit>,
    selected: usize,
    skill_ids: Vec<String>,
    scroll: usize,
    visible_rows: usize,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl SkillsUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(SkillsFocus::Skills);
        focus.register(SkillsFocus::Reload);
        focus.register(SkillsFocus::Close);
        focus.set(SkillsFocus::Skills);
        Self {
            open: false,
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            selected: 0,
            skill_ids: Vec::new(),
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
        self.ensure_visible(total);
        self.focus.set(SkillsFocus::Skills);
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
    pub fn focused(&self) -> Option<SkillsFocus> {
        self.focus.current().copied()
    }

    pub fn focus_hit(&mut self, hit: SkillsHit) {
        match hit {
            SkillsHit::Skill(index) => {
                self.selected = index;
                self.focus.set(SkillsFocus::Skills);
            }
            SkillsHit::Reload => self.focus.set(SkillsFocus::Reload),
            SkillsHit::Close => self.focus.set(SkillsFocus::Close),
        }
    }

    pub fn next_skill(&mut self, total: usize) {
        if total > 0 {
            self.selected = self.selected.saturating_add(1).min(total - 1);
            self.ensure_visible(total);
        }
    }

    pub fn previous_skill(&mut self, total: usize) {
        if total > 0 {
            self.selected = self.selected.saturating_sub(1);
            self.ensure_visible(total);
        }
    }

    pub fn page_skills(&mut self, total: usize, forward: bool) {
        if total == 0 {
            return;
        }
        self.selected = if forward {
            self.selected
                .saturating_add(self.visible_rows.max(1))
                .min(total - 1)
        } else {
            self.selected.saturating_sub(self.visible_rows.max(1))
        };
        self.ensure_visible(total);
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.selected
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<SkillsHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn sync(&mut self, snapshot: &SkillCatalogSnapshot) {
        let selected_id = self.skill_ids.get(self.selected).cloned();
        self.selected = self.selected.min(snapshot.skills.len().saturating_sub(1));
        if let Some(index) =
            selected_id.and_then(|id| snapshot.skills.iter().position(|skill| skill.id == id))
        {
            self.selected = index;
        }
        self.skill_ids = snapshot
            .skills
            .iter()
            .map(|skill| skill.id.clone())
            .collect();
        self.ensure_visible(snapshot.skills.len());
        self.clicks.clear();
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &SkillCatalogSnapshot, editable: bool) {
        if !self.open {
            return;
        }
        self.sync(snapshot);
        let focused = self.focus.current().copied();
        let selected = self.selected;
        let scroll = self.scroll;
        let animation_frame = self.animation_frame;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::AgentSkills))
            .width_percent(84)
            .height_percent(84)
            .min_size(72, 24)
            .max_size(148, 54)
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
        self.ensure_visible(snapshot.skills.len());
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
}

impl Default for SkillsUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &SkillCatalogSnapshot,
    focused: Option<SkillsFocus>,
    selected: usize,
    scroll: usize,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<SkillsHit>,
) -> usize {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(2),
        Constraint::Min(7),
        Constraint::Length(6),
        Constraint::Length(4),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["◐", "◓", "◑", "◒"][animation_frame % 4];
    let active = snapshot.skills.iter().filter(|skill| skill.enabled).count();
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {active}/{} {} · {} {}",
                    snapshot.skills.len(),
                    text(Text::EnabledLabel),
                    text(Text::Revision),
                    snapshot.revision
                ),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::SkillMetadataHelp)),
        ]),
        rows[0],
    );

    let budget_ratio = if snapshot.metadata_budget_bytes == 0 {
        0.0
    } else {
        snapshot.metadata_bytes_used as f64 / snapshot.metadata_budget_bytes as f64
    };
    frame.render_widget(
        Gauge::default()
            .ratio(budget_ratio.clamp(0.0, 1.0))
            .label(format!(
                "{} {} / {} B{}",
                text(Text::MetadataLabel),
                snapshot.metadata_bytes_used,
                snapshot.metadata_budget_bytes,
                if snapshot.metadata_omitted == 0 {
                    String::new()
                } else {
                    format!(
                        " · {} {}",
                        snapshot.metadata_omitted,
                        text(Text::OmittedLabel)
                    )
                }
            ))
            .gauge_style(Style::default().fg(if snapshot.metadata_omitted == 0 {
                Color::Cyan
            } else {
                Color::Yellow
            })),
        rows[1],
    );

    let list_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(SkillsFocus::Skills) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(format!(" {} ", text(Text::SkillsNavigationHelp)));
    let inner = list_block.inner(rows[2]);
    frame.render_widget(list_block, rows[2]);
    let visible = usize::from(inner.height);
    for (line_index, (index, skill)) in snapshot
        .skills
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
        render_skill(
            frame,
            row,
            skill,
            index == selected && focused == Some(SkillsFocus::Skills),
            editable,
            index,
            clicks,
        );
    }

    let details = snapshot.skills.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoValidSkills))],
        |skill| {
            vec![
                Line::from(vec![
                    Span::styled(
                        format!("{}: ", text(Text::NameLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        truncate_for_display(&sanitize_for_display(&skill.name), 256),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" · {}: ", text(Text::SourceLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(skill.source.to_string()),
                    Span::styled(
                        format!(" · {}: ", text(Text::ResourcesLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(skill.resource_count.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!("{}: ", text(Text::IdentifierLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(truncate_for_display(
                        &sanitize_for_display(&skill.id),
                        1_024,
                    )),
                ]),
                Line::from(vec![
                    Span::styled(
                        format!("{}: ", text(Text::PathLabel)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(truncate_for_display(
                        &sanitize_for_display(&skill.display_path),
                        1_024,
                    )),
                ]),
                Line::from(truncate_for_display(
                    &sanitize_for_display(&skill.description),
                    2_048,
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(details)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::SelectedSkill))),
            )
            .wrap(Wrap { trim: false }),
        rows[3],
    );

    let diagnostic_lines = if snapshot.diagnostics.is_empty() {
        vec![Line::from(Span::styled(
            format!("✓ {}", text(Text::NoSkillDiagnostics)),
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
                        truncate_for_display(&sanitize_for_display(diagnostic), 1_000)
                    ),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(diagnostic_lines)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · {} ",
                text(Text::DiagnosticsLabel),
                snapshot.diagnostics.len()
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
        text(Text::ReloadCatalog),
        SkillsHit::Reload,
        focused == Some(SkillsFocus::Reload),
        editable,
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::CloseEsc),
        SkillsHit::Close,
        focused == Some(SkillsFocus::Close),
        true,
        clicks,
    );
    visible.max(1)
}

fn render_skill(
    frame: &mut Frame<'_>,
    area: Rect,
    skill: &SkillSummary,
    focused: bool,
    enabled: bool,
    index: usize,
    clicks: &mut ClickRegionRegistry<SkillsHit>,
) {
    let mut state = CheckBoxState::new(skill.enabled);
    state.set_enabled(enabled);
    state.set_focused(focused);
    let label = format!(
        "{} · {} · {} {}",
        truncate_for_display(&sanitize_for_display(&skill.name), 96),
        skill.source,
        skill.resource_count,
        text(Text::ResourceCount)
    );
    let region = CheckBox::new(&label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::EnabledLabel), text(Text::DisabledLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, SkillsHit::Skill(index));
    }
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: SkillsHit,
    focused: bool,
    enabled: bool,
    clicks: &mut ClickRegionRegistry<SkillsHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(if hit == SkillsHit::Reload {
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

    use super::{SkillsFocus, SkillsHit, SkillsUiState};
    use crate::agent::{SkillCatalogSnapshot, SkillSource, SkillSummary};

    fn skill(id: &str, name: &str) -> SkillSummary {
        SkillSummary {
            id: id.to_owned(),
            name: name.to_owned(),
            description: "Check ownership and cancellation paths".to_owned(),
            source: SkillSource::Project,
            display_path: format!(".decode/skills/{id}/SKILL.md"),
            enabled: true,
            resource_count: 2,
        }
    }

    fn snapshot() -> SkillCatalogSnapshot {
        SkillCatalogSnapshot {
            revision: 3,
            skills: Arc::from([skill("project:rust-review", "Rust review")]),
            diagnostics: Arc::from(["one malformed skill was ignored".to_owned()]),
            metadata_budget_bytes: 4_096,
            metadata_bytes_used: 512,
            metadata_omitted: 0,
        }
    }

    #[test]
    fn dialog_has_real_skill_and_button_hit_regions() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = snapshot();
        let mut state = SkillsUiState::new();
        state.open(snapshot.skills.len());
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;
        let rendered = terminal.backend().to_string();
        assert!(rendered.contains("Agent Skills"));
        assert!(rendered.contains("Rust review"));
        assert!(rendered.contains("one malformed skill"));

        let mut found_skill = false;
        let mut found_reload = false;
        let mut found_close = false;
        for row in 0..40 {
            for column in 0..120 {
                match state.clicked(column, row) {
                    Some(SkillsHit::Skill(0)) => found_skill = true,
                    Some(SkillsHit::Reload) => found_reload = true,
                    Some(SkillsHit::Close) => found_close = true,
                    Some(SkillsHit::Skill(_)) | None => {}
                }
            }
        }
        assert!(found_skill && found_reload && found_close);
        Ok(())
    }

    #[test]
    fn tab_fallback_cycles_all_controls() {
        let mut state = SkillsUiState::new();
        assert_eq!(state.focused(), Some(SkillsFocus::Skills));
        state.next_focus();
        assert_eq!(state.focused(), Some(SkillsFocus::Reload));
        state.next_focus();
        assert_eq!(state.focused(), Some(SkillsFocus::Close));
        state.previous_focus();
        assert_eq!(state.focused(), Some(SkillsFocus::Reload));
    }

    #[test]
    fn selection_follows_the_skill_when_catalog_order_changes() {
        let first = SkillCatalogSnapshot {
            skills: Arc::from([skill("first", "First"), skill("second", "Second")]),
            ..SkillCatalogSnapshot::default()
        };
        let second = SkillCatalogSnapshot {
            revision: 1,
            skills: Arc::from([skill("second", "Second"), skill("first", "First")]),
            ..SkillCatalogSnapshot::default()
        };
        let mut state = SkillsUiState::new();
        state.open(first.skills.len());
        state.sync(&first);
        state.focus_hit(SkillsHit::Skill(1));

        state.sync(&second);

        assert_eq!(state.selected(), 0);
    }

    #[test]
    fn catalog_update_clears_stale_skill_hit_regions() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = snapshot();
        let mut state = SkillsUiState::new();
        state.open(snapshot.skills.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;

        state.sync(&SkillCatalogSnapshot::default());

        assert!(!(0..40).any(|row| {
            (0..120).any(|column| matches!(state.clicked(column, row), Some(SkillsHit::Skill(_))))
        }));
        Ok(())
    }
}
