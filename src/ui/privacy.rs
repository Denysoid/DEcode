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
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};

use crate::privacy::{PrivacySnapshot, PrivacySourceSnapshot};

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

const ANIMATION_STEP: Duration = Duration::from_millis(160);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivacyFocus {
    Sources,
    Reload,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrivacyHit {
    Source(usize),
    Reload,
    Close,
}

#[derive(Debug, Clone)]
pub struct PrivacyUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<PrivacyFocus>,
    clicks: ClickRegionRegistry<PrivacyHit>,
    source_ids: Vec<&'static str>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl PrivacyUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(PrivacyFocus::Sources);
        focus.register(PrivacyFocus::Reload);
        focus.register(PrivacyFocus::Close);
        focus.set(PrivacyFocus::Sources);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            focus,
            clicks: ClickRegionRegistry::new(),
            source_ids: Vec::new(),
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
        self.picker.set_total(total);
        self.picker.select_first();
        self.source_ids.clear();
        self.focus.set(PrivacyFocus::Sources);
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

    pub fn sync(&mut self, snapshot: &PrivacySnapshot) {
        let selected_id = self.source_ids.get(self.picker.selected_index).copied();
        self.picker.set_total(snapshot.sources.len());
        if let Some(index) =
            selected_id.and_then(|id| snapshot.sources.iter().position(|source| source.id == id))
        {
            self.picker.select(index);
        } else if !snapshot.sources.is_empty()
            && self.picker.selected_index >= snapshot.sources.len()
        {
            self.picker.select(snapshot.sources.len().saturating_sub(1));
        }
        self.source_ids = snapshot.sources.iter().map(|source| source.id).collect();
        self.clicks.clear();
    }

    pub fn next(&mut self) {
        self.picker.select_next();
    }

    pub fn previous(&mut self) {
        self.picker.select_prev();
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.focus.set(PrivacyFocus::Sources);
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: PrivacyFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<PrivacyFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PrivacyHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &PrivacySnapshot, editable: bool) {
        if !self.open {
            return;
        }
        self.sync(snapshot);
        let selected = self.picker.selected_index;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::PrivacyShield))
            .width_percent(82)
            .height_percent(70)
            .min_size(66, 18)
            .max_size(150, 44)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                snapshot,
                selected,
                picker,
                focused,
                animation_frame,
                editable,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for PrivacyUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &PrivacySnapshot,
    selected: usize,
    picker: &mut ListPickerState,
    focused: Option<PrivacyFocus>,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<PrivacyHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(area);
    let shield = ["[| ]", "[ |]", "[# ]", "[ #]"][animation_frame % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("{shield} {}", text(Text::PrivacyActive)),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  |  {} {}  |  {} {}",
                    snapshot.blocked_attempts,
                    text(Text::BlockedAttempts),
                    text(Text::PolicyFingerprint),
                    snapshot
                        .policy_sha256
                        .get(..10)
                        .unwrap_or(text(Text::Unavailable))
                )),
            ]),
            Line::from(text(Text::PrivacySourcesHelp)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let columns =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(rows[1]);
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::RuleSources)));
    let list_inner = list_block.inner(columns[0]);
    frame.render_widget(list_block, columns[0]);
    let labels = snapshot
        .sources
        .iter()
        .map(source_label)
        .collect::<Vec<_>>();
    let viewport = usize::from(list_inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        list_inner,
    );
    for row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= snapshot.sources.len() {
            break;
        }
        clicks.register(
            Rect::new(
                list_inner.x,
                list_inner.y.saturating_add(row as u16),
                list_inner.width,
                1,
            ),
            PrivacyHit::Source(index),
        );
    }

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::EffectivePolicy)));
    let detail_inner = detail_block.inner(columns[1]);
    frame.render_widget(detail_block, columns[1]);
    frame.render_widget(
        Paragraph::new(detail_lines(snapshot.sources.get(selected))).wrap(Wrap { trim: false }),
        detail_inner,
    );

    let buttons = Layout::horizontal([
        Constraint::Length(22),
        Constraint::Fill(1),
        Constraint::Length(16),
    ])
    .split(rows[2]);
    render_button(
        frame,
        buttons[0],
        text(Text::ReloadRules),
        PrivacyHit::Reload,
        focused == Some(PrivacyFocus::Reload),
        editable,
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::CloseEsc),
        PrivacyHit::Close,
        focused == Some(PrivacyFocus::Close),
        true,
        clicks,
    );
}

fn source_label(source: &PrivacySourceSnapshot) -> String {
    let icon = if source.fail_closed {
        "X"
    } else if source.active {
        "+"
    } else {
        "-"
    };
    format!(
        "{icon} {}  |  {} {}",
        sanitize_for_display(source.label),
        source.rule_count,
        text(Text::RuleCount)
    )
}

fn detail_lines(source: Option<&PrivacySourceSnapshot>) -> Vec<Line<'static>> {
    let Some(source) = source else {
        return vec![Line::from(text(Text::NoPolicySources))];
    };
    let state = if source.fail_closed {
        text(Text::FailClosedAllPaths)
    } else if source.active {
        text(Text::SourceActive)
    } else {
        text(Text::OptionalFileAbsent)
    };
    vec![
        Line::from(Span::styled(
            sanitize_for_display(source.label),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("{}: {state}", text(Text::StateLabel))),
        Line::from(format!("{}: {}", text(Text::Rules), source.rule_count)),
        Line::from(format!(
            "{}: {}",
            text(Text::LocationLabel),
            sanitize_for_display(&source.location)
        )),
        Line::from(""),
        Line::from(sanitize_for_display(&source.detail)),
        Line::from(""),
        Line::from(text(Text::MalformedSourcesHelp)),
        Line::from(text(Text::ShellApprovalBoundary)),
    ]
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: PrivacyHit,
    focused: bool,
    enabled: bool,
    clicks: &mut ClickRegionRegistry<PrivacyHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(if hit == PrivacyHit::Reload {
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

    use crate::privacy::{PrivacySnapshot, PrivacySourceSnapshot};

    use super::{PrivacyFocus, PrivacyHit, PrivacyUiState};

    fn source(id: &'static str, label: &'static str) -> PrivacySourceSnapshot {
        PrivacySourceSnapshot {
            id,
            label,
            location: "compiled".to_owned(),
            active: true,
            fail_closed: false,
            rule_count: 1,
            detail: "active".to_owned(),
        }
    }

    #[test]
    fn selected_source_follows_identity_across_reordering() {
        let first = PrivacySnapshot {
            revision: 1,
            policy_sha256: String::new(),
            blocked_attempts: 0,
            sources: Arc::from([source("built-in", "Built-in"), source("user", "User")]),
        };
        let reordered = PrivacySnapshot {
            revision: 2,
            policy_sha256: String::new(),
            blocked_attempts: 0,
            sources: Arc::from([source("user", "User"), source("built-in", "Built-in")]),
        };
        let mut state = PrivacyUiState::new();
        state.open(first.sources.len());
        state.sync(&first);
        state.select(1);

        state.sync(&reordered);

        assert_eq!(state.picker.selected_index, 0);
    }

    #[test]
    fn sources_reload_and_close_have_real_mouse_regions() -> Result<(), Box<dyn std::error::Error>>
    {
        let snapshot = PrivacySnapshot {
            revision: 1,
            policy_sha256: "0123456789abcdef".to_owned(),
            blocked_attempts: 2,
            sources: Arc::from([PrivacySourceSnapshot {
                id: "built-in",
                label: "Built-in",
                location: "compiled".to_owned(),
                active: true,
                fail_closed: false,
                rule_count: 4,
                detail: "active".to_owned(),
            }]),
        };
        let mut state = PrivacyUiState::new();
        state.open(snapshot.sources.len());
        assert_eq!(state.focused(), Some(PrivacyFocus::Sources));
        state.next_focus();
        assert_eq!(state.focused(), Some(PrivacyFocus::Reload));
        state.next_focus();
        assert_eq!(state.focused(), Some(PrivacyFocus::Close));
        state.previous_focus();
        assert_eq!(state.focused(), Some(PrivacyFocus::Reload));
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw(frame, &snapshot, true))?;
        let mut source = false;
        let mut reload = false;
        let mut close = false;
        for row in 0..30 {
            for column in 0..100 {
                match state.clicked(column, row) {
                    Some(PrivacyHit::Source(0)) => source = true,
                    Some(PrivacyHit::Reload) => reload = true,
                    Some(PrivacyHit::Close) => close = true,
                    Some(PrivacyHit::Source(_)) | None => {}
                }
            }
        }
        assert!(source && reload && close);
        Ok(())
    }
}
