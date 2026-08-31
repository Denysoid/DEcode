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

use crate::agent::{
    ReviewCatalogSnapshot, ReviewFinding, ReviewFindingDisposition, ReviewReport, ReviewSeverity,
    ReviewVerdict,
};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
    syntax,
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewFocus {
    PreviousReport,
    Findings,
    NextReport,
    Accept,
    QueueFix,
    Dismiss,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewHit {
    PreviousReport,
    Finding(usize),
    NextReport,
    Accept,
    QueueFix,
    Dismiss,
    Close,
}

#[derive(Debug, Clone)]
pub struct ReviewUiState {
    open: bool,
    dialog: DialogState<()>,
    focus: FocusManager<ReviewFocus>,
    picker: ListPickerState,
    clicks: ClickRegionRegistry<ReviewHit>,
    report_index: usize,
    finding_index: usize,
    detail_scroll: u16,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl ReviewUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            ReviewFocus::PreviousReport,
            ReviewFocus::Findings,
            ReviewFocus::NextReport,
            ReviewFocus::Accept,
            ReviewFocus::QueueFix,
            ReviewFocus::Dismiss,
            ReviewFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(ReviewFocus::Findings);
        Self {
            open: false,
            dialog: DialogState::new(()),
            focus,
            picker: ListPickerState::new(0),
            clicks: ClickRegionRegistry::new(),
            report_index: 0,
            finding_index: 0,
            detail_scroll: 0,
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, snapshot: &ReviewCatalogSnapshot) {
        self.open = true;
        self.report_index = snapshot.reports.len().saturating_sub(1);
        self.select_first_open(snapshot);
        self.focus.set(ReviewFocus::Findings);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn sync(&mut self, snapshot: &ReviewCatalogSnapshot) {
        self.report_index = self
            .report_index
            .min(snapshot.reports.len().saturating_sub(1));
        let finding_count = self
            .report(snapshot)
            .map_or(0, |report| report.findings.len());
        self.finding_index = self.finding_index.min(finding_count.saturating_sub(1));
        self.picker.set_total(finding_count);
        if finding_count == 0 {
            self.picker.select_first();
        }
        self.picker.select(self.finding_index);
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
    pub fn focused(&self) -> Option<ReviewFocus> {
        self.focus.current().copied()
    }

    pub fn focus_hit(&mut self, hit: ReviewHit) {
        match hit {
            ReviewHit::PreviousReport => self.focus.set(ReviewFocus::PreviousReport),
            ReviewHit::Finding(index) => {
                self.focus.set(ReviewFocus::Findings);
                self.finding_index = index;
                self.picker.select(index);
                self.detail_scroll = 0;
            }
            ReviewHit::NextReport => self.focus.set(ReviewFocus::NextReport),
            ReviewHit::Accept => self.focus.set(ReviewFocus::Accept),
            ReviewHit::QueueFix => self.focus.set(ReviewFocus::QueueFix),
            ReviewHit::Dismiss => self.focus.set(ReviewFocus::Dismiss),
            ReviewHit::Close => self.focus.set(ReviewFocus::Close),
        }
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<ReviewHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn select_previous_finding(&mut self) {
        self.finding_index = self.finding_index.saturating_sub(1);
        self.picker.select(self.finding_index);
        self.detail_scroll = 0;
    }

    pub fn select_next_finding(&mut self, snapshot: &ReviewCatalogSnapshot) {
        let max = self
            .report(snapshot)
            .map_or(0, |report| report.findings.len().saturating_sub(1));
        self.finding_index = self.finding_index.saturating_add(1).min(max);
        self.picker.select(self.finding_index);
        self.detail_scroll = 0;
    }

    pub fn previous_report(&mut self, snapshot: &ReviewCatalogSnapshot) {
        self.report_index = self.report_index.saturating_sub(1);
        self.select_first_open(snapshot);
    }

    pub fn next_report(&mut self, snapshot: &ReviewCatalogSnapshot) {
        self.report_index = self
            .report_index
            .saturating_add(1)
            .min(snapshot.reports.len().saturating_sub(1));
        self.select_first_open(snapshot);
    }

    pub fn scroll_detail(&mut self, delta: i16) {
        if delta.is_negative() {
            self.detail_scroll = self.detail_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta as u16);
        }
    }

    #[must_use]
    pub fn report<'a>(&self, snapshot: &'a ReviewCatalogSnapshot) -> Option<&'a ReviewReport> {
        snapshot.reports.get(self.report_index)
    }

    #[must_use]
    pub fn finding<'a>(&self, snapshot: &'a ReviewCatalogSnapshot) -> Option<&'a ReviewFinding> {
        self.report(snapshot)
            .and_then(|report| report.findings.get(self.finding_index))
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &ReviewCatalogSnapshot, idle: bool) {
        if !self.open {
            return;
        }
        let focused = self.focused();
        let report_index = self.report_index;
        let finding_index = self.finding_index;
        let detail_scroll = &mut self.detail_scroll;
        let animation_frame = self.animation_frame;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::ImmutableCodeReview))
            .width_percent(88)
            .height_percent(88)
            .min_size(76, 28)
            .max_size(156, 62)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_review(
                frame,
                area,
                snapshot,
                report_index,
                finding_index,
                detail_scroll,
                focused,
                animation_frame,
                idle,
                picker,
                clicks,
            );
        });
        popup.render(frame);
    }

    fn select_first_open(&mut self, snapshot: &ReviewCatalogSnapshot) {
        let report = snapshot.reports.get(self.report_index);
        self.picker
            .set_total(report.map_or(0, |report| report.findings.len()));
        self.finding_index = report
            .and_then(|report| {
                report
                    .findings
                    .iter()
                    .position(|finding| finding.disposition == ReviewFindingDisposition::Open)
            })
            .unwrap_or(0);
        if self.picker.total_items == 0 {
            self.picker.select_first();
        }
        self.picker.select(self.finding_index);
        self.detail_scroll = 0;
    }
}

impl Default for ReviewUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_review(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &ReviewCatalogSnapshot,
    report_index: usize,
    finding_index: usize,
    detail_scroll: &mut u16,
    focused: Option<ReviewFocus>,
    animation_frame: usize,
    idle: bool,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<ReviewHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Min(10),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["·", "›", "◆", "›"][animation_frame % 4];
    let Some(report) = snapshot.reports.get(report_index) else {
        frame.render_widget(
            Paragraph::new(text(Text::NoStructuredReviewReport))
                .alignment(ratatui::layout::Alignment::Center)
                .wrap(Wrap { trim: false }),
            Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(rows[4].height),
            ),
        );
        draw_button(
            frame,
            Layout::horizontal([Constraint::Length(20), Constraint::Fill(1)]).split(rows[4])[0],
            text(Text::CloseEsc),
            ReviewHit::Close,
            focused == Some(ReviewFocus::Close),
            true,
            ButtonStyle::default(),
            clicks,
        );
        return;
    };
    let digest = report
        .snapshot_sha256
        .get(..12)
        .unwrap_or(&report.snapshot_sha256);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!(
                        "{pulse} {} #{} · {}",
                        text(Text::ReviewLabel),
                        report.id,
                        verdict_label(report.verdict)
                    ),
                    Style::default()
                        .fg(verdict_color(report))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    " · {} {} · {} {}",
                    report.findings.len(),
                    text(Text::FindingsLabel),
                    report.changed_paths.len(),
                    text(Text::PathsLabel)
                )),
            ]),
            Line::from(format!(
                "{} {digest}… · {} {} · {} {} · {} {}",
                text(Text::SnapshotLabel),
                report.diff_bytes,
                text(Text::Bytes),
                text(Text::Turn),
                report.turn_id,
                text(Text::ReportRevisionLabel),
                report.revision
            )),
            Line::from(truncate_for_display(
                &sanitize_for_display(&report.summary),
                2_000,
            )),
            Line::from(if idle {
                text(Text::ReviewIdleDecisionHelp)
            } else {
                text(Text::ReviewBusyDecisionHelp)
            }),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    draw_report_navigation(
        frame,
        rows[1],
        snapshot.reports.len(),
        report_index,
        focused,
        clicks,
    );
    draw_findings(
        frame,
        rows[2],
        report,
        finding_index,
        focused,
        picker,
        clicks,
    );
    draw_detail(
        frame,
        rows[3],
        report.findings.get(finding_index),
        detail_scroll,
    );
    draw_actions(
        frame,
        rows[4],
        report.findings.get(finding_index),
        idle,
        focused,
        clicks,
    );
}

fn draw_report_navigation(
    frame: &mut Frame<'_>,
    area: Rect,
    total: usize,
    selected: usize,
    focused: Option<ReviewFocus>,
    clicks: &mut ClickRegionRegistry<ReviewHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Length(20),
        Constraint::Fill(1),
        Constraint::Length(20),
    ])
    .split(area);
    draw_button(
        frame,
        columns[0],
        text(Text::OlderReport),
        ReviewHit::PreviousReport,
        focused == Some(ReviewFocus::PreviousReport),
        selected > 0,
        ButtonStyle::default(),
        clicks,
    );
    frame.render_widget(
        Paragraph::new(format!(
            "{} {} / {}",
            text(Text::ReviewLabel),
            selected.saturating_add(1).min(total),
            total
        ))
        .alignment(ratatui::layout::Alignment::Center),
        columns[1],
    );
    draw_button(
        frame,
        columns[2],
        text(Text::NewerReport),
        ReviewHit::NextReport,
        focused == Some(ReviewFocus::NextReport),
        selected.saturating_add(1) < total,
        ButtonStyle::default(),
        clicks,
    );
}

fn draw_findings(
    frame: &mut Frame<'_>,
    area: Rect,
    report: &ReviewReport,
    _selected: usize,
    focused: Option<ReviewFocus>,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<ReviewHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(ReviewFocus::Findings) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(
            " {} ({}) ",
            text(Text::FindingsLabel),
            report.findings.len()
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if report.findings.is_empty() {
        frame.render_widget(
            Paragraph::new(text(Text::NoConcreteDefects)).style(Style::default().fg(Color::Green)),
            inner,
        );
        return;
    }
    let labels = report
        .findings
        .iter()
        .map(|finding| {
            sanitize_for_display(&format!(
                "[{}] [{}] {}:{} · {}",
                severity_label(finding.severity),
                disposition_label(finding.disposition),
                finding.path,
                finding
                    .line_start
                    .map_or_else(|| "?".to_owned(), |line| line.to_string()),
                finding.title
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
        if index >= report.findings.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            ReviewHit::Finding(index),
        );
    }
}

fn draw_detail(
    frame: &mut Frame<'_>,
    area: Rect,
    finding: Option<&ReviewFinding>,
    scroll: &mut u16,
) {
    let Some(finding) = finding else {
        frame.render_widget(
            Paragraph::new(text(Text::NoFindingSelected)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::EvidenceLabel))),
            ),
            area,
        );
        return;
    };
    // Security invariant: terminal controls and bidi overrides are converted
    // to visible escapes before syntax highlighting is allowed to inspect text.
    let safe_title = sanitize_for_display(&finding.title);
    let safe_body = sanitize_for_display(&finding.body);
    let safe_fix = sanitize_for_display(&finding.suggested_fix);
    let range = match (finding.line_start, finding.line_end) {
        (Some(start), Some(end)) if start != end => format!("{start}-{end}"),
        (Some(line), _) => line.to_string(),
        _ => "?".to_owned(),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            truncate_for_display(&safe_title, 1_000),
            Style::default()
                .fg(severity_color(finding.severity))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}:{} · {} {} · {} {}",
            sanitize_for_display(&finding.path),
            range,
            text(Text::SeverityLabel),
            severity_label(finding.severity),
            text(Text::StateLabel),
            disposition_label(finding.disposition)
        )),
        Line::from(""),
        Line::from(truncate_for_display(&safe_body, 16_000)),
        Line::from(""),
        Line::from(Span::styled(
            text(Text::SuggestedDirection),
            Style::default().fg(Color::LightCyan),
        )),
    ];
    if safe_fix.is_empty() {
        lines.push(Line::from(text(Text::NoDirectFixSuggested)));
    } else if let Some(highlighted) = syntax::highlight_source(&finding.path, &safe_fix) {
        lines.extend(highlighted.into_iter().map(Line::from));
    } else {
        lines.push(Line::from(truncate_for_display(&safe_fix, 16_000)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::EvidenceLabel)));
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width.max(1))
        .saturating_sub(usize::from(inner.height))
        .min(usize::from(u16::MAX)) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(paragraph.block(block).scroll((*scroll, 0)), area);
}

fn draw_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    finding: Option<&ReviewFinding>,
    idle: bool,
    focused: Option<ReviewFocus>,
    clicks: &mut ClickRegionRegistry<ReviewHit>,
) {
    let open = finding.is_some_and(|finding| finding.disposition == ReviewFindingDisposition::Open);
    let enabled = idle && open;
    let columns = Layout::horizontal([
        Constraint::Length(17),
        Constraint::Length(1),
        Constraint::Length(22),
        Constraint::Length(1),
        Constraint::Length(17),
        Constraint::Fill(1),
        Constraint::Length(16),
    ])
    .split(area);
    draw_button(
        frame,
        columns[0],
        text(Text::AcceptFinding),
        ReviewHit::Accept,
        focused == Some(ReviewFocus::Accept),
        enabled,
        ButtonStyle::success(),
        clicks,
    );
    draw_button(
        frame,
        columns[2],
        text(Text::QueueSafeFix),
        ReviewHit::QueueFix,
        focused == Some(ReviewFocus::QueueFix),
        enabled,
        ButtonStyle::primary(),
        clicks,
    );
    draw_button(
        frame,
        columns[4],
        text(Text::DismissFinding),
        ReviewHit::Dismiss,
        focused == Some(ReviewFocus::Dismiss),
        enabled,
        ButtonStyle::danger(),
        clicks,
    );
    draw_button(
        frame,
        columns[6],
        text(Text::CloseEsc),
        ReviewHit::Close,
        focused == Some(ReviewFocus::Close),
        true,
        ButtonStyle::default(),
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: ReviewHit,
    focused: bool,
    enabled: bool,
    style: ButtonStyle,
    clicks: &mut ClickRegionRegistry<ReviewHit>,
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

fn verdict_label(verdict: ReviewVerdict) -> &'static str {
    match verdict {
        ReviewVerdict::Pass => text(Text::PassVerdict),
        ReviewVerdict::ChangesRequested => text(Text::ChangesRequestedVerdict),
    }
}

fn severity_label(severity: ReviewSeverity) -> &'static str {
    match severity {
        ReviewSeverity::Low => text(Text::Low),
        ReviewSeverity::Medium => text(Text::Medium),
        ReviewSeverity::High => text(Text::High),
        ReviewSeverity::Critical => text(Text::CriticalLabel),
    }
}

fn disposition_label(disposition: ReviewFindingDisposition) -> &'static str {
    match disposition {
        ReviewFindingDisposition::Open => text(Text::OpenFindingLabel),
        ReviewFindingDisposition::Accepted => text(Text::AcceptedFindingLabel),
        ReviewFindingDisposition::Dismissed => text(Text::DismissedFindingLabel),
        ReviewFindingDisposition::FixQueued => text(Text::FixQueuedLabel),
    }
}

fn verdict_color(report: &ReviewReport) -> Color {
    if report.findings.is_empty() {
        Color::Green
    } else {
        Color::Yellow
    }
}

const fn severity_color(severity: ReviewSeverity) -> Color {
    match severity {
        ReviewSeverity::Low => Color::Blue,
        ReviewSeverity::Medium => Color::Yellow,
        ReviewSeverity::High => Color::LightRed,
        ReviewSeverity::Critical => Color::Red,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::agent::{ReviewVerdict, review::ReviewFindingDisposition};

    fn snapshot() -> ReviewCatalogSnapshot {
        ReviewCatalogSnapshot {
            revision: 1,
            reports: Arc::from([ReviewReport {
                id: 1,
                revision: 1,
                turn_id: 7,
                snapshot_sha256: "a".repeat(64),
                changed_paths: vec!["src/lib.rs".to_owned()],
                diff_bytes: 100,
                verdict: ReviewVerdict::ChangesRequested,
                summary: "One issue".to_owned(),
                findings: vec![ReviewFinding {
                    id: 3,
                    severity: ReviewSeverity::High,
                    title: "Unsafe escape \u{1b}[31m".to_owned(),
                    body: "Bidi \u{202e} evidence".to_owned(),
                    path: "src/lib.rs".to_owned(),
                    line_start: Some(4),
                    line_end: Some(4),
                    suggested_fix: "fn fixed() {}".to_owned(),
                    disposition: ReviewFindingDisposition::Open,
                    decided_at: None,
                }],
                created_at: Utc::now(),
            }]),
        }
    }

    #[test]
    fn popup_exposes_mouse_regions_and_sanitizes_before_highlighting()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = snapshot();
        let mut ui = ReviewUiState::new();
        ui.open(&snapshot);
        let mut terminal = Terminal::new(TestBackend::new(120, 42))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;
        for expected in [
            ReviewHit::Finding(0),
            ReviewHit::Accept,
            ReviewHit::QueueFix,
            ReviewHit::Dismiss,
            ReviewHit::Close,
        ] {
            let found =
                (0..42).any(|row| (0..120).any(|column| ui.clicked(column, row) == Some(expected)));
            assert!(found, "missing click region for {expected:?}");
        }
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("\\x1b"));
        Ok(())
    }

    #[test]
    fn keyboard_selection_updates_the_visible_finding_picker() {
        let mut report = snapshot().reports[0].clone();
        let mut second = report.findings[0].clone();
        second.id = 4;
        second.title = "Second finding".to_owned();
        report.findings.push(second);
        let snapshot = ReviewCatalogSnapshot {
            revision: 2,
            reports: Arc::from([report]),
        };
        let mut ui = ReviewUiState::new();
        ui.open(&snapshot);

        ui.select_next_finding(&snapshot);

        assert_eq!(ui.finding_index, 1);
        assert_eq!(ui.picker.selected_index, 1);
    }

    #[test]
    fn drawing_short_details_clamps_stale_scroll() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = snapshot();
        let mut ui = ReviewUiState::new();
        ui.open(&snapshot);
        ui.scroll_detail(i16::MAX);
        let mut terminal = Terminal::new(TestBackend::new(120, 42))?;

        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;

        assert_eq!(ui.detail_scroll, 0);
        Ok(())
    }

    #[test]
    fn decisions_have_no_click_region_while_agent_is_busy() -> Result<(), Box<dyn std::error::Error>>
    {
        let snapshot = snapshot();
        let mut ui = ReviewUiState::new();
        ui.open(&snapshot);
        let mut terminal = Terminal::new(TestBackend::new(120, 42))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, false))?;
        let decision = (0..42).any(|row| {
            (0..120).any(|column| {
                matches!(
                    ui.clicked(column, row),
                    Some(ReviewHit::Accept | ReviewHit::QueueFix | ReviewHit::Dismiss)
                )
            })
        });
        assert!(!decision);
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_reach_every_review_control() {
        let mut ui = ReviewUiState::new();
        for expected in [
            ReviewFocus::Findings,
            ReviewFocus::NextReport,
            ReviewFocus::Accept,
            ReviewFocus::QueueFix,
            ReviewFocus::Dismiss,
            ReviewFocus::Close,
            ReviewFocus::PreviousReport,
        ] {
            assert_eq!(ui.focused(), Some(expected));
            ui.next_focus();
        }
        assert_eq!(ui.focused(), Some(ReviewFocus::Findings));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(ReviewFocus::PreviousReport));
    }

    #[test]
    fn empty_review_still_has_a_mouse_close_button() -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = ReviewCatalogSnapshot::default();
        let mut ui = ReviewUiState::new();
        ui.open(&snapshot);
        let mut terminal = Terminal::new(TestBackend::new(100, 34))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;
        let found = (0..34)
            .any(|row| (0..100).any(|column| ui.clicked(column, row) == Some(ReviewHit::Close)));
        assert!(found, "empty review popup has no mouse close control");
        Ok(())
    }
}
