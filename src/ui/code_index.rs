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
        Button, ButtonState, ButtonStyle, ButtonVariant, DialogConfig, DialogState, ListPicker,
        ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    agent::side_chat::has_visible_text,
    code_index::{CodeIndexHit, CodeIndexSnapshot, CodeIndexState},
};

use super::{
    i18n::{Text, notice_text, text},
    render::sanitize_for_display,
    syntax,
};

const ANIMATION_STEP: Duration = Duration::from_millis(120);
const MAX_UI_QUERY_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeIndexFocus {
    Query,
    Results,
    Close,
    Refresh,
    Rebuild,
    Cancel,
    Search,
    Mention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeIndexHitRegion {
    Query,
    Result(usize),
    Close,
    Refresh,
    Rebuild,
    Cancel,
    Search,
    Mention,
}

#[derive(Debug, Clone)]
pub struct CodeIndexUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<CodeIndexFocus>,
    clicks: ClickRegionRegistry<CodeIndexHitRegion>,
    query: String,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl CodeIndexUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            CodeIndexFocus::Query,
            CodeIndexFocus::Results,
            CodeIndexFocus::Close,
            CodeIndexFocus::Refresh,
            CodeIndexFocus::Rebuild,
            CodeIndexFocus::Cancel,
            CodeIndexFocus::Search,
            CodeIndexFocus::Mention,
        ] {
            focus.register(item);
        }
        focus.set(CodeIndexFocus::Query);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            focus,
            clicks: ClickRegionRegistry::new(),
            query: String::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, results: usize) {
        self.open = true;
        self.set_results(results);
        self.focus.set(CodeIndexFocus::Query);
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

    pub fn set_results(&mut self, total: usize) {
        self.picker.set_total(total);
        if total > 0 && self.picker.selected_index >= total {
            self.picker.select(total.saturating_sub(1));
        }
    }

    #[must_use]
    pub const fn selected_result(&self) -> usize {
        self.picker.selected_index
    }

    pub fn select_result(&mut self, index: usize) {
        self.picker.select(index);
    }

    pub fn next_result(&mut self) {
        self.picker.select_next();
    }

    pub fn previous_result(&mut self) {
        self.picker.select_prev();
    }

    pub fn first_result(&mut self) {
        self.picker.select_first();
    }

    pub fn last_result(&mut self) {
        self.picker.select_last();
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: CodeIndexFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<CodeIndexFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn push_char(&mut self, character: char) {
        if !character.is_control()
            && self.query.len().saturating_add(character.len_utf8()) <= MAX_UI_QUERY_BYTES
        {
            self.query.push(character);
        }
    }

    pub fn push_text(&mut self, text: &str) {
        for character in text.chars() {
            if character.is_control() {
                continue;
            }
            if self.query.len().saturating_add(character.len_utf8()) > MAX_UI_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
    }

    pub fn pop_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
        }
    }

    pub fn clear_query(&mut self) {
        self.query.clear();
    }

    #[must_use]
    pub fn query_has_visible_text(&self) -> bool {
        has_visible_text(&self.query)
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<CodeIndexHitRegion> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        snapshot: &CodeIndexSnapshot,
        hits: &[CodeIndexHit],
    ) {
        if !self.open {
            return;
        }
        self.set_results(hits.len());
        let config = DialogConfig::new(text(Text::RepositoryIntelligence))
            .width_percent(90)
            .height_percent(84)
            .min_size(78, 24)
            .max_size(180, 64)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightMagenta)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let selected = self.picker.selected_index;
        let query = self.query.clone();
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_content(
                frame,
                area,
                snapshot,
                hits,
                &query,
                focused,
                animation_frame,
                selected,
                picker,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for CodeIndexUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &CodeIndexSnapshot,
    hits: &[CodeIndexHit],
    query: &str,
    focused: Option<CodeIndexFocus>,
    animation_frame: usize,
    selected: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<CodeIndexHitRegion>,
) {
    let rows = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(area);
    let capability = if snapshot.embeddings_enabled {
        text(Text::HybridSearchHelp)
    } else {
        text(Text::LocalIndexSearchHelp)
    };
    frame.render_widget(
        Paragraph::new(capability).wrap(Wrap { trim: false }),
        rows[0],
    );
    draw_status(frame, rows[1], snapshot, animation_frame);
    draw_query(frame, rows[2], query, focused, clicks);
    draw_results(frame, rows[3], hits, selected, picker, clicks);
    draw_actions(
        frame,
        rows[4],
        snapshot,
        hits.get(selected),
        query,
        focused,
        clicks,
    );
}

fn draw_status(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &CodeIndexSnapshot,
    animation_frame: usize,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(area);
    let status = format!(
        "{} {}  |  {} {}  |  {} {} / {} {}\n{}\n{}: {} {} / {} {} — {}",
        state_icon(snapshot.state, animation_frame),
        state_label(snapshot.state),
        text(Text::GenerationLabel),
        snapshot.generation,
        snapshot.indexed_files,
        text(Text::FilesLabel),
        snapshot.chunk_count,
        text(Text::ChunksLabel),
        sanitize_for_display(&notice_text(&snapshot.notice)),
        text(Text::VectorLabel),
        snapshot.embedded_chunks,
        text(Text::EmbeddedLabel),
        snapshot.vector_cache_bytes,
        text(Text::Bytes),
        sanitize_for_display(&notice_text(&snapshot.embedding_notice)),
    );
    frame.render_widget(
        Paragraph::new(status)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::IndexStatusTitle))),
            )
            .style(Style::default().fg(state_color(snapshot.state)))
            .wrap(Wrap { trim: false }),
        columns[0],
    );
    let ratio = if snapshot.total_files == 0 {
        if snapshot.state == CodeIndexState::Ready {
            1.0
        } else {
            0.0
        }
    } else {
        snapshot.scanned_files.min(snapshot.total_files) as f64 / snapshot.total_files as f64
    };
    let label = format!(
        "{} / {}  |  {} {}  {} {}  {} {}",
        snapshot.scanned_files,
        snapshot.total_files,
        text(Text::ReusedLabel),
        snapshot.reused_files,
        text(Text::ChangedLabel),
        snapshot.changed_files,
        text(Text::SkippedLabel),
        snapshot.skipped_files
    );
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::RefreshProgressTitle))),
            )
            .gauge_style(Style::default().fg(Color::LightMagenta))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(label),
        columns[1],
    );
}

fn draw_query(
    frame: &mut Frame<'_>,
    area: Rect,
    query: &str,
    focused: Option<CodeIndexFocus>,
    clicks: &mut ClickRegionRegistry<CodeIndexHitRegion>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::NaturalLanguageQueryTitle)))
        .border_style(if focused == Some(CodeIndexFocus::Query) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default()
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let line = if query.is_empty() {
        Line::from(Span::styled(
            text(Text::QueryPlaceholder),
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(sanitize_for_display(query))
    };
    frame.render_widget(Paragraph::new(line), inner);
    clicks.register(area, CodeIndexHitRegion::Query);
}

fn draw_results(
    frame: &mut Frame<'_>,
    area: Rect,
    hits: &[CodeIndexHit],
    selected: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<CodeIndexHitRegion>,
) {
    let columns =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(area);
    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} ({}) ",
        text(Text::RankedResults),
        hits.len()
    ));
    let inner = block.inner(columns[0]);
    frame.render_widget(block, columns[0]);
    let labels = hits
        .iter()
        .map(|hit| {
            sanitize_for_display(&format!(
                "{:.2}  {}:{}-{}  {}",
                hit.score,
                hit.path,
                hit.start_line,
                hit.end_line,
                hit.symbols.first().map_or("", String::as_str)
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
        if index >= hits.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            CodeIndexHitRegion::Result(index),
        );
    }

    let detail = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::SanitizedPreviewTitle)));
    let detail_inner = detail.inner(columns[1]);
    frame.render_widget(detail, columns[1]);
    let lines = hits.get(selected).map_or_else(
        || vec![Line::from(text(Text::NoSearchResultsHelp))],
        |hit| {
            let sanitized = sanitize_for_display(&hit.snippet);
            let mut lines = vec![
                Line::from(Span::styled(
                    format!(
                        "{}:{}-{}",
                        sanitize_for_display(&hit.path),
                        hit.start_line,
                        hit.end_line
                    ),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(format!(
                    "{} {:.3}  |  {}: {}",
                    text(Text::ScoreLabel),
                    hit.score,
                    text(Text::SymbolsLabel),
                    if hit.symbols.is_empty() {
                        "-".to_owned()
                    } else {
                        hit.symbols
                            .iter()
                            .map(|symbol| sanitize_for_display(symbol))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                )),
                Line::from(""),
            ];
            if let Some(highlighted) = syntax::highlight_source(&hit.path, &sanitized) {
                lines.extend(highlighted.into_iter().map(Line::from));
            } else {
                lines.extend(sanitized.lines().map(|line| Line::from(line.to_owned())));
            }
            lines
        },
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        detail_inner,
    );
}

fn draw_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &CodeIndexSnapshot,
    selected: Option<&CodeIndexHit>,
    query: &str,
    focused: Option<CodeIndexFocus>,
    clicks: &mut ClickRegionRegistry<CodeIndexHitRegion>,
) {
    let columns = Layout::horizontal([
        Constraint::Length(12),
        Constraint::Fill(1),
        Constraint::Length(16),
        Constraint::Length(16),
        Constraint::Length(14),
        Constraint::Length(16),
        Constraint::Length(22),
    ])
    .split(area);
    render_button(
        frame,
        columns[0],
        text(Text::Close),
        CodeIndexHitRegion::Close,
        focused == Some(CodeIndexFocus::Close),
        true,
        ButtonStyle::default(),
        clicks,
    );
    let building = snapshot.state == CodeIndexState::Building;
    render_button(
        frame,
        columns[2],
        text(Text::RefreshLabel),
        CodeIndexHitRegion::Refresh,
        focused == Some(CodeIndexFocus::Refresh),
        snapshot.runtime_available && !building,
        ButtonStyle::primary(),
        clicks,
    );
    render_button(
        frame,
        columns[3],
        text(Text::FullRebuild),
        CodeIndexHitRegion::Rebuild,
        focused == Some(CodeIndexFocus::Rebuild),
        snapshot.runtime_available && !building,
        ButtonStyle::default(),
        clicks,
    );
    render_button(
        frame,
        columns[4],
        text(Text::Cancel),
        CodeIndexHitRegion::Cancel,
        focused == Some(CodeIndexFocus::Cancel),
        building,
        ButtonStyle::danger(),
        clicks,
    );
    render_button(
        frame,
        columns[5],
        text(Text::SearchLabel),
        CodeIndexHitRegion::Search,
        focused == Some(CodeIndexFocus::Search),
        snapshot.state == CodeIndexState::Ready && has_visible_text(query),
        ButtonStyle::primary(),
        clicks,
    );
    render_button(
        frame,
        columns[6],
        text(Text::MentionInChat),
        CodeIndexHitRegion::Mention,
        focused == Some(CodeIndexFocus::Mention),
        selected.is_some(),
        ButtonStyle::primary(),
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: CodeIndexHitRegion,
    focused: bool,
    enabled: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<CodeIndexHitRegion>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(style)
        .render_stateful(area, frame.buffer_mut());
    if enabled {
        clicks.register(region.area, hit);
    }
}

fn state_icon(state: CodeIndexState, frame: usize) -> &'static str {
    match state {
        CodeIndexState::Building | CodeIndexState::Loading => ["◐", "◓", "◑", "◒"][frame % 4],
        CodeIndexState::Ready => "●",
        CodeIndexState::Error => "!",
        CodeIndexState::Cancelled => "■",
        CodeIndexState::Empty => "○",
        CodeIndexState::Disabled => "×",
    }
}

pub(crate) fn state_label(state: CodeIndexState) -> &'static str {
    match state {
        CodeIndexState::Disabled => text(Text::DisabledLabel),
        CodeIndexState::Empty => text(Text::Empty),
        CodeIndexState::Loading => text(Text::LoadingLabel),
        CodeIndexState::Building => text(Text::BuildingLabel),
        CodeIndexState::Ready => text(Text::Ready),
        CodeIndexState::Cancelled => text(Text::Cancelled),
        CodeIndexState::Error => text(Text::ErrorLabel),
    }
}

const fn state_color(state: CodeIndexState) -> Color {
    match state {
        CodeIndexState::Ready => Color::Green,
        CodeIndexState::Building | CodeIndexState::Loading => Color::LightMagenta,
        CodeIndexState::Error => Color::Red,
        CodeIndexState::Cancelled => Color::Yellow,
        CodeIndexState::Empty => Color::Gray,
        CodeIndexState::Disabled => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use ratatui::{Terminal, backend::TestBackend};

    use super::{
        CodeIndexHit, CodeIndexHitRegion, CodeIndexSnapshot, CodeIndexState, CodeIndexUiState,
        state_icon,
    };

    fn snapshot(state: CodeIndexState) -> CodeIndexSnapshot {
        let mut snapshot = CodeIndexSnapshot::new(true);
        snapshot.state = state;
        snapshot.total_files = 10;
        snapshot.scanned_files = 5;
        snapshot.indexed_files = 4;
        snapshot.chunk_count = 7;
        snapshot
    }

    fn hit() -> CodeIndexHit {
        CodeIndexHit {
            path: "src/auth.rs".to_owned(),
            start_line: 10,
            end_line: 20,
            score: 9.5,
            symbols: vec!["verify_token".to_owned()],
            snippet: "fn verify_token() {}\n".to_owned(),
        }
    }

    #[test]
    fn building_indicator_animates_without_changing_ready_semantics() {
        assert_ne!(
            state_icon(CodeIndexState::Building, 0),
            state_icon(CodeIndexState::Building, 1)
        );
        assert_eq!(
            state_icon(CodeIndexState::Ready, 0),
            state_icon(CodeIndexState::Ready, 100)
        );
    }

    #[test]
    fn ready_search_and_result_have_real_mouse_regions() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = CodeIndexUiState::new();
        ui.open(1);
        ui.push_text("authentication");
        let mut terminal = Terminal::new(TestBackend::new(130, 40))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot(CodeIndexState::Ready), &[hit()]))?;
        let area = terminal.backend().buffer().area;
        for expected in [
            CodeIndexHitRegion::Query,
            CodeIndexHitRegion::Result(0),
            CodeIndexHitRegion::Close,
            CodeIndexHitRegion::Refresh,
            CodeIndexHitRegion::Rebuild,
            CodeIndexHitRegion::Search,
            CodeIndexHitRegion::Mention,
        ] {
            let found = (0..area.height)
                .any(|row| (0..area.width).any(|column| ui.clicked(column, row) == Some(expected)));
            if !found {
                return Err(io::Error::other(format!("missing hit region {expected:?}")).into());
            }
        }
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_reach_every_index_control() {
        let mut ui = CodeIndexUiState::new();
        let expected = [
            super::CodeIndexFocus::Query,
            super::CodeIndexFocus::Results,
            super::CodeIndexFocus::Close,
            super::CodeIndexFocus::Refresh,
            super::CodeIndexFocus::Rebuild,
            super::CodeIndexFocus::Cancel,
            super::CodeIndexFocus::Search,
            super::CodeIndexFocus::Mention,
        ];
        for focus in expected {
            assert_eq!(ui.focused(), Some(focus));
            ui.next_focus();
        }
        assert_eq!(ui.focused(), Some(super::CodeIndexFocus::Query));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(super::CodeIndexFocus::Mention));
    }

    #[test]
    fn building_state_exposes_cancel_but_not_refresh_or_search()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = CodeIndexUiState::new();
        ui.open(0);
        ui.push_text("auth");
        let mut terminal = Terminal::new(TestBackend::new(130, 40))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot(CodeIndexState::Building), &[]))?;
        let area = terminal.backend().buffer().area;
        let has = |expected| {
            (0..area.height)
                .any(|row| (0..area.width).any(|column| ui.clicked(column, row) == Some(expected)))
        };
        assert!(has(CodeIndexHitRegion::Cancel));
        assert!(!has(CodeIndexHitRegion::Refresh));
        assert!(!has(CodeIndexHitRegion::Search));
        Ok(())
    }

    #[test]
    fn query_paste_stops_before_an_incomplete_utf8_character() {
        let mut ui = CodeIndexUiState::new();
        ui.push_text(&"a".repeat(super::MAX_UI_QUERY_BYTES - 1));

        ui.push_text("💻b");

        assert_eq!(ui.query().len(), super::MAX_UI_QUERY_BYTES - 1);
        assert!(ui.query().ends_with('a'));
    }

    #[test]
    fn invisible_query_does_not_enable_search() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = CodeIndexUiState::new();
        ui.open(0);
        ui.push_text("\u{200b}\u{200d}");
        let mut terminal = Terminal::new(TestBackend::new(130, 40))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot(CodeIndexState::Ready), &[]))?;
        let area = terminal.backend().buffer().area;

        assert!(!(0..area.height).any(|row| {
            (0..area.width)
                .any(|column| ui.clicked(column, row) == Some(CodeIndexHitRegion::Search))
        }));
        Ok(())
    }

    #[test]
    fn result_detail_sanitizes_all_symbol_names() -> Result<(), Box<dyn std::error::Error>> {
        let mut malicious = hit();
        malicious.symbols.push("hidden\u{202e}symbol".to_owned());
        let mut ui = CodeIndexUiState::new();
        ui.open(1);
        let mut terminal = Terminal::new(TestBackend::new(130, 40))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot(CodeIndexState::Ready), &[malicious]))?;
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("hidden<U+202E>symbol"));
        Ok(())
    }
}
