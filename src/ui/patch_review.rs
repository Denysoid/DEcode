use std::sync::Arc;

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
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, PopupDialog,
    },
    state::FocusManager,
};
use similar::{ChangeTag, TextDiff};

use crate::tools::PatchReview;

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
    syntax,
};

const PREVIEW_GRAPHEMES: usize = 24_000;
const PREVIEW_LINES: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchReviewFocus {
    RejectHunk,
    AcceptHunk,
    RejectAll,
    Cancel,
    Apply,
    AcceptAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PatchReviewHit {
    Hunk(usize),
    Action(PatchReviewFocus),
}

#[derive(Debug, Clone)]
pub struct PatchReviewUiState {
    dialog: DialogState<()>,
    focus: FocusManager<PatchReviewFocus>,
    clicks: ClickRegionRegistry<PatchReviewHit>,
    selected_hunk: usize,
    decisions: Vec<Option<bool>>,
}

impl PatchReviewUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(PatchReviewFocus::RejectHunk);
        focus.register(PatchReviewFocus::AcceptHunk);
        focus.register(PatchReviewFocus::RejectAll);
        focus.register(PatchReviewFocus::Cancel);
        focus.register(PatchReviewFocus::Apply);
        focus.register(PatchReviewFocus::AcceptAll);
        focus.set(PatchReviewFocus::AcceptHunk);
        Self {
            dialog: DialogState::new(()),
            focus,
            clicks: ClickRegionRegistry::new(),
            selected_hunk: 0,
            decisions: Vec::new(),
        }
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn open(&mut self, hunk_count: usize) {
        self.selected_hunk = 0;
        self.decisions = vec![None; hunk_count];
        self.focus.set(PatchReviewFocus::AcceptHunk);
        self.clicks.clear();
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.dialog.hide();
        self.clicks.clear();
        self.decisions.clear();
        self.selected_hunk = 0;
    }

    #[must_use]
    pub const fn selected_hunk(&self) -> usize {
        self.selected_hunk
    }

    pub fn select_hunk(&mut self, index: usize) {
        if index < self.decisions.len() {
            self.selected_hunk = index;
        }
    }

    pub fn next_hunk(&mut self) {
        if !self.decisions.is_empty() {
            self.selected_hunk = self
                .selected_hunk
                .saturating_add(1)
                .min(self.decisions.len().saturating_sub(1));
        }
    }

    pub fn previous_hunk(&mut self) {
        self.selected_hunk = self.selected_hunk.saturating_sub(1);
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: PatchReviewFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<PatchReviewFocus> {
        self.focus.current().copied()
    }

    pub fn decide_current(&mut self, approved: bool) {
        let Some(decision) = self.decisions.get_mut(self.selected_hunk) else {
            return;
        };
        *decision = Some(approved);
        if let Some(next) = self
            .decisions
            .iter()
            .enumerate()
            .skip(self.selected_hunk.saturating_add(1))
            .find_map(|(index, decision)| decision.is_none().then_some(index))
            .or_else(|| {
                self.decisions
                    .iter()
                    .position(|decision| decision.is_none())
            })
        {
            self.selected_hunk = next;
        }
    }

    pub fn decide_all(&mut self, approved: bool) {
        self.decisions.fill(Some(approved));
    }

    #[must_use]
    pub fn completed(&self) -> usize {
        self.decisions
            .iter()
            .filter(|decision| decision.is_some())
            .count()
    }

    #[must_use]
    pub fn decisions(&self) -> Option<Vec<bool>> {
        self.decisions.iter().copied().collect()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PatchReviewHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, review: &Arc<PatchReview>) {
        if !self.dialog.is_visible() {
            self.dialog.show();
        }
        if self.decisions.len() != review.hunks.len() {
            self.open(review.hunks.len());
        }
        self.clicks.clear();
        let selected_hunk = self.selected_hunk;
        let decisions = self.decisions.clone();
        let focused = self.focus.current().copied();
        let config = DialogConfig::new(text(Text::ReviewPatchHunks))
            .width_percent(94)
            .height_percent(90)
            .min_size(64, 18)
            .max_size(210, 70)
            .border_color(Color::Yellow)
            .focused_border_color(Color::LightYellow)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                review,
                selected_hunk,
                &decisions,
                focused,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for PatchReviewUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    review: &PatchReview,
    selected_hunk: usize,
    decisions: &[Option<bool>],
    focused: Option<PatchReviewFocus>,
    clicks: &mut ClickRegionRegistry<PatchReviewHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Length(7),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                sanitize_for_display(&review.path),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                "{} {}/{} | {}",
                text(Text::ReviewedLabel),
                decisions
                    .iter()
                    .filter(|decision| decision.is_some())
                    .count(),
                decisions.len(),
                text(Text::PatchHunkHelp)
            )),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let body = Layout::horizontal([Constraint::Length(24), Constraint::Min(20)]).split(rows[1]);
    draw_hunk_list(frame, body[0], selected_hunk, decisions, clicks);
    draw_hunk_preview(frame, body[1], review, selected_hunk);
    draw_actions(frame, rows[2], decisions, focused, clicks);
}

fn draw_hunk_list(
    frame: &mut Frame<'_>,
    area: Rect,
    selected_hunk: usize,
    decisions: &[Option<bool>],
    clicks: &mut ClickRegionRegistry<PatchReviewHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::HunksLabel)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let viewport = usize::from(inner.height);
    let start = selected_hunk
        .saturating_sub(viewport.saturating_sub(1))
        .min(decisions.len().saturating_sub(viewport));
    let lines = decisions
        .iter()
        .enumerate()
        .skip(start)
        .take(viewport)
        .map(|(index, decision)| {
            let state = match decision {
                Some(true) => text(Text::AcceptLabel),
                Some(false) => text(Text::Reject),
                None => text(Text::Pending),
            };
            let style = if index == selected_hunk {
                Style::default().fg(Color::Black).bg(Color::LightYellow)
            } else {
                match decision {
                    Some(true) => Style::default().fg(Color::Green),
                    Some(false) => Style::default().fg(Color::Red),
                    None => Style::default().fg(Color::Gray),
                }
            };
            Line::from(Span::styled(format!("#{:<4} {state}", index + 1), style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), inner);
    for row in 0..viewport {
        let index = start.saturating_add(row);
        if index >= decisions.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            PatchReviewHit::Hunk(index),
        );
    }
}

fn draw_hunk_preview(frame: &mut Frame<'_>, area: Rect, review: &PatchReview, selected: usize) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::SanitizedDiffTitle)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let Some(hunk) = review.hunks.get(selected) else {
        frame.render_widget(Paragraph::new(text(Text::NoChangedHunks)), inner);
        return;
    };

    // Security order is intentional: terminal controls and bidi controls are
    // neutralized before untrusted model text reaches the syntax highlighter.
    let old = truncate_for_display(&sanitize_for_display(&hunk.old), PREVIEW_GRAPHEMES);
    let new = truncate_for_display(&sanitize_for_display(&hunk.new), PREVIEW_GRAPHEMES);
    let old_highlights = syntax::highlight_source(&review.path, &old);
    let new_highlights = syntax::highlight_source(&review.path, &new);
    let diff = TextDiff::from_lines(&old, &new);
    let mut lines = Vec::new();
    for change in diff.iter_all_changes().take(PREVIEW_LINES) {
        let (sign, color, highlighted) = match change.tag() {
            ChangeTag::Delete => (
                "-",
                Color::Red,
                change
                    .old_index()
                    .and_then(|index| old_highlights.as_ref().and_then(|rows| rows.get(index))),
            ),
            ChangeTag::Insert => (
                "+",
                Color::Green,
                change
                    .new_index()
                    .and_then(|index| new_highlights.as_ref().and_then(|rows| rows.get(index))),
            ),
            ChangeTag::Equal => (
                " ",
                Color::DarkGray,
                change
                    .new_index()
                    .and_then(|index| new_highlights.as_ref().and_then(|rows| rows.get(index))),
            ),
        };
        let mut spans = vec![Span::styled(
            sign,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )];
        if let Some(highlighted) = highlighted {
            spans.extend(highlighted.iter().cloned());
        } else {
            spans.push(Span::styled(
                change.value().trim_end_matches('\n').to_owned(),
                Style::default().fg(color),
            ));
        }
        lines.push(Line::from(spans));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    decisions: &[Option<bool>],
    focused: Option<PatchReviewFocus>,
    clicks: &mut ClickRegionRegistry<PatchReviewHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
    ])
    .split(area);
    let current = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(24),
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Fill(1),
    ])
    .split(rows[0]);
    draw_button(
        frame,
        current[1],
        text(Text::RejectHunk),
        PatchReviewFocus::RejectHunk,
        focused,
        true,
        ButtonStyle::danger(),
        clicks,
    );
    draw_button(
        frame,
        current[3],
        text(Text::AcceptHunk),
        PatchReviewFocus::AcceptHunk,
        focused,
        true,
        ButtonStyle::success(),
        clicks,
    );
    frame.render_widget(
        Paragraph::new(text(Text::PatchNavigationHelp)).style(Style::default().fg(Color::DarkGray)),
        rows[1],
    );
    let all = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(rows[2]);
    draw_button(
        frame,
        all[0],
        text(Text::RejectAll),
        PatchReviewFocus::RejectAll,
        focused,
        true,
        ButtonStyle::danger(),
        clicks,
    );
    draw_button(
        frame,
        all[1],
        text(Text::CancelPatch),
        PatchReviewFocus::Cancel,
        focused,
        true,
        ButtonStyle::danger(),
        clicks,
    );
    draw_button(
        frame,
        all[2],
        text(Text::ApplyDecisions),
        PatchReviewFocus::Apply,
        focused,
        decisions.iter().all(Option::is_some),
        ButtonStyle::primary(),
        clicks,
    );
    draw_button(
        frame,
        all[3],
        text(Text::AcceptAll),
        PatchReviewFocus::AcceptAll,
        focused,
        true,
        ButtonStyle::success(),
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    action: PatchReviewFocus,
    focused: Option<PatchReviewFocus>,
    enabled: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<PatchReviewHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused == Some(action));
    let button = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style);
    let region = button.render_stateful(area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, PatchReviewHit::Action(action));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

    use super::{PatchReviewFocus, PatchReviewHit, PatchReviewUiState};
    use crate::tools::PatchReview;

    #[test]
    fn apply_is_unavailable_until_every_hunk_has_a_decision() {
        let mut state = PatchReviewUiState::new();
        state.open(2);
        assert!(state.decisions().is_none());
        state.decide_current(true);
        assert!(state.decisions().is_none());
        state.decide_current(false);
        assert_eq!(state.decisions(), Some(vec![true, false]));
    }

    #[test]
    fn bulk_decision_remains_explicit_until_submit() {
        let mut state = PatchReviewUiState::new();
        state.open(3);
        state.decide_all(false);
        assert_eq!(state.decisions(), Some(vec![false, false, false]));
    }

    #[test]
    fn cancel_patch_has_a_real_mouse_hit_region() -> Result<(), Box<dyn std::error::Error>> {
        let review = Arc::new(PatchReview::new(
            "src/lib.rs",
            "fn old() {}\n",
            "fn new() {}\n",
        ));
        let mut state = PatchReviewUiState::new();
        state.open(review.hunks.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw(frame, &review))?;
        assert!((0..36).any(|row| {
            (0..120).any(|column| {
                state.clicked(column, row) == Some(PatchReviewHit::Action(PatchReviewFocus::Cancel))
            })
        }));
        Ok(())
    }

    #[test]
    fn every_enabled_patch_action_has_a_mouse_hit_region() -> Result<(), Box<dyn std::error::Error>>
    {
        let review = Arc::new(PatchReview::new(
            "src/lib.rs",
            "fn old() {}\n",
            "fn new() {}\n",
        ));
        let mut state = PatchReviewUiState::new();
        state.open(review.hunks.len());
        state.decide_all(true);
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw(frame, &review))?;

        for action in [
            PatchReviewFocus::RejectHunk,
            PatchReviewFocus::AcceptHunk,
            PatchReviewFocus::RejectAll,
            PatchReviewFocus::Cancel,
            PatchReviewFocus::Apply,
            PatchReviewFocus::AcceptAll,
        ] {
            assert!((0..36).any(|row| {
                (0..120).any(|column| {
                    state.clicked(column, row) == Some(PatchReviewHit::Action(action))
                })
            }));
        }
        Ok(())
    }

    #[test]
    fn preview_sanitizes_terminal_controls_before_highlighting()
    -> Result<(), Box<dyn std::error::Error>> {
        let review = Arc::new(PatchReview::new(
            "src/lib.rs",
            "fn old() {}\n",
            "\u{1b}[31mfn new() {}\u{202e}\n",
        ));
        let mut state = PatchReviewUiState::new();
        state.open(review.hunks.len());
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| state.draw(frame, &review))?;
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("\\x1b"));
        Ok(())
    }
}
