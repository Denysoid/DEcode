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
use unicode_segmentation::UnicodeSegmentation;

use crate::{
    agent::{SideChatSnapshot, SideExchange, SideExchangeStatus, side_chat::has_visible_text},
    api::ReasoningEffort,
};

use super::{
    i18n::{Text, notice_text, text},
    render::{sanitize_for_display, truncate_for_display},
    syntax,
};

const ANIMATION_STEP: Duration = Duration::from_millis(140);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideStage {
    Closed,
    Compose,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideFocus {
    Question,
    Model,
    Effort,
    History,
    Primary,
    Secondary,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SideHit {
    Question,
    Model(usize),
    Effort(usize),
    History(usize),
    Primary,
    Secondary,
    Close,
}

#[derive(Debug, Clone)]
pub struct SideChatUiState {
    stage: SideStage,
    dialog: DialogState<()>,
    question: String,
    model_picker: ListPickerState,
    model_choices: Vec<String>,
    effort_picker: ListPickerState,
    history_picker: ListPickerState,
    exchange_ids: Vec<u64>,
    awaiting_exchange: bool,
    focus: FocusManager<SideFocus>,
    clicks: ClickRegionRegistry<SideHit>,
    answer_scroll: u16,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl SideChatUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            SideFocus::Question,
            SideFocus::Model,
            SideFocus::Effort,
            SideFocus::History,
            SideFocus::Primary,
            SideFocus::Secondary,
            SideFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(SideFocus::Question);
        Self {
            stage: SideStage::Closed,
            dialog: DialogState::new(()),
            question: String::new(),
            model_picker: ListPickerState::new(0),
            model_choices: Vec::new(),
            effort_picker: ListPickerState::new(5),
            history_picker: ListPickerState::new(0),
            exchange_ids: Vec::new(),
            awaiting_exchange: false,
            focus,
            clicks: ClickRegionRegistry::new(),
            answer_scroll: 0,
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, SideStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> SideStage {
        self.stage
    }

    #[must_use]
    pub fn focused(&self) -> Option<SideFocus> {
        self.focus.current().copied()
    }

    pub fn open(
        &mut self,
        snapshot: &SideChatSnapshot,
        models: &[String],
        current_model: &str,
        current_effort: ReasoningEffort,
    ) {
        self.model_picker.set_total(models.len());
        self.model_choices = models.to_vec();
        self.awaiting_exchange = false;
        self.model_picker.select(
            models
                .iter()
                .position(|model| model == current_model)
                .unwrap_or(0),
        );
        self.effort_picker.select(effort_index(current_effort));
        self.sync(snapshot);
        if !snapshot.exchanges.is_empty() {
            self.history_picker
                .select(snapshot.exchanges.len().saturating_sub(1));
        }
        self.stage = if snapshot.exchanges.is_empty() {
            SideStage::Compose
        } else {
            SideStage::Transcript
        };
        self.focus.set(if matches!(self.stage, SideStage::Compose) {
            SideFocus::Question
        } else {
            SideFocus::History
        });
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = SideStage::Closed;
        self.dialog.hide();
        self.clicks.clear();
        self.answer_scroll = 0;
        self.awaiting_exchange = false;
    }

    pub fn compose(&mut self) {
        self.stage = SideStage::Compose;
        self.question.clear();
        self.answer_scroll = 0;
        self.awaiting_exchange = false;
        self.focus.set(SideFocus::Question);
    }

    pub fn show_transcript(&mut self) {
        self.stage = SideStage::Transcript;
        self.answer_scroll = 0;
        self.awaiting_exchange = false;
        self.focus.set(SideFocus::History);
    }

    pub fn mark_submitted(&mut self) {
        self.question.clear();
        self.stage = SideStage::Transcript;
        self.answer_scroll = 0;
        self.awaiting_exchange = true;
        self.focus.set(SideFocus::History);
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

    pub fn sync(&mut self, snapshot: &SideChatSnapshot) {
        let selected_id = self
            .exchange_ids
            .get(self.history_picker.selected_index)
            .copied();
        let received_new_exchange = self.awaiting_exchange
            && snapshot.exchanges.last().map(|exchange| exchange.id)
                != self.exchange_ids.last().copied();
        self.history_picker.set_total(snapshot.exchanges.len());
        if received_new_exchange && !snapshot.exchanges.is_empty() {
            self.history_picker.select_last();
            self.awaiting_exchange = false;
        } else if let Some(index) = selected_id.and_then(|id| {
            snapshot
                .exchanges
                .iter()
                .position(|exchange| exchange.id == id)
        }) {
            self.history_picker.select(index);
        }
        self.exchange_ids = snapshot
            .exchanges
            .iter()
            .map(|exchange| exchange.id)
            .collect();
        self.clicks.clear();
    }

    pub fn next_focus(&mut self) {
        let next = match (self.stage, self.focused()) {
            (SideStage::Compose, Some(SideFocus::Question)) => SideFocus::Model,
            (SideStage::Compose, Some(SideFocus::Model)) => SideFocus::Effort,
            (SideStage::Compose, Some(SideFocus::Effort)) => SideFocus::Primary,
            (SideStage::Compose, Some(SideFocus::Primary)) => SideFocus::Secondary,
            (SideStage::Compose, Some(SideFocus::Secondary)) => SideFocus::Close,
            (SideStage::Compose, _) => SideFocus::Question,
            (SideStage::Transcript, Some(SideFocus::History)) => SideFocus::Primary,
            (SideStage::Transcript, Some(SideFocus::Primary)) => SideFocus::Secondary,
            (SideStage::Transcript, Some(SideFocus::Secondary)) => SideFocus::Close,
            (SideStage::Transcript, _) => SideFocus::History,
            (SideStage::Closed, _) => SideFocus::Close,
        };
        self.focus.set(next);
    }

    pub fn previous_focus(&mut self) {
        let previous = match (self.stage, self.focused()) {
            (SideStage::Compose, Some(SideFocus::Question)) => SideFocus::Close,
            (SideStage::Compose, Some(SideFocus::Model)) => SideFocus::Question,
            (SideStage::Compose, Some(SideFocus::Effort)) => SideFocus::Model,
            (SideStage::Compose, Some(SideFocus::Primary)) => SideFocus::Effort,
            (SideStage::Compose, Some(SideFocus::Secondary)) => SideFocus::Primary,
            (SideStage::Compose, _) => SideFocus::Secondary,
            (SideStage::Transcript, Some(SideFocus::History)) => SideFocus::Close,
            (SideStage::Transcript, Some(SideFocus::Primary)) => SideFocus::History,
            (SideStage::Transcript, Some(SideFocus::Secondary)) => SideFocus::Primary,
            (SideStage::Transcript, _) => SideFocus::Secondary,
            (SideStage::Closed, _) => SideFocus::Close,
        };
        self.focus.set(previous);
    }

    pub fn focus(&mut self, focus: SideFocus) {
        self.focus.set(focus);
    }

    pub fn next_item(&mut self) {
        match self.focused() {
            Some(SideFocus::Model) => self.model_picker.select_next(),
            Some(SideFocus::Effort) => self.effort_picker.select_next(),
            Some(SideFocus::History) => {
                self.history_picker.select_next();
                self.answer_scroll = 0;
            }
            _ => {}
        }
    }

    pub fn previous_item(&mut self) {
        match self.focused() {
            Some(SideFocus::Model) => self.model_picker.select_prev(),
            Some(SideFocus::Effort) => self.effort_picker.select_prev(),
            Some(SideFocus::History) => {
                self.history_picker.select_prev();
                self.answer_scroll = 0;
            }
            _ => {}
        }
    }

    pub fn select_model(&mut self, index: usize) {
        self.model_picker.select(index);
        self.focus.set(SideFocus::Model);
    }

    pub fn select_effort(&mut self, index: usize) {
        self.effort_picker.select(index);
        self.focus.set(SideFocus::Effort);
    }

    pub fn select_history(&mut self, index: usize) {
        self.history_picker.select(index);
        self.answer_scroll = 0;
        self.focus.set(SideFocus::History);
    }

    #[must_use]
    pub const fn selected_model_index(&self) -> usize {
        self.model_picker.selected_index
    }

    #[must_use]
    pub fn selected_model(&self) -> Option<&str> {
        self.model_choices
            .get(self.model_picker.selected_index)
            .map(String::as_str)
    }

    #[must_use]
    pub const fn selected_effort(&self) -> ReasoningEffort {
        effort_from_index(self.effort_picker.selected_index)
    }

    #[must_use]
    pub fn selected_exchange<'a>(
        &self,
        snapshot: &'a SideChatSnapshot,
    ) -> Option<&'a SideExchange> {
        snapshot.exchanges.get(self.history_picker.selected_index)
    }

    #[must_use]
    pub fn question(&self) -> &str {
        &self.question
    }

    pub fn push_char(&mut self, character: char) {
        if (character == '\n' || character == '\t' || !character.is_control())
            && self.question.len().saturating_add(character.len_utf8())
                <= crate::agent::side_chat::MAX_SIDE_QUESTION_BYTES
        {
            self.question.push(character);
        }
    }

    pub fn push_text(&mut self, text: &str) {
        for character in text.chars() {
            if self.question.len() >= crate::agent::side_chat::MAX_SIDE_QUESTION_BYTES {
                break;
            }
            self.push_char(character);
        }
    }

    pub fn pop_char(&mut self) {
        if let Some((index, _)) =
            UnicodeSegmentation::grapheme_indices(self.question.as_str(), true).next_back()
        {
            self.question.truncate(index);
        }
    }

    pub fn scroll_answer(&mut self, delta: i16) {
        if delta.is_negative() {
            self.answer_scroll = self.answer_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.answer_scroll = self.answer_scroll.saturating_add(delta.unsigned_abs());
        }
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<SideHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &SideChatSnapshot, models: &[String]) {
        if !self.is_open() {
            return;
        }
        self.sync(snapshot);
        self.sync_models(models);
        let stage = self.stage;
        let focused = self.focused();
        let selected_history = self.history_picker.selected_index;
        let mut answer_scroll = self.answer_scroll;
        let animation_frame = self.animation_frame;
        let model_picker = &mut self.model_picker;
        let effort_picker = &mut self.effort_picker;
        let history_picker = &mut self.history_picker;
        let clicks = &mut self.clicks;
        let question = self.question.clone();
        let config = DialogConfig::new(match stage {
            SideStage::Compose => text(Text::AskReadOnlySideQuestion),
            SideStage::Transcript => text(Text::SideQuestionsSeparate),
            SideStage::Closed => text(Text::SideQuestions),
        })
        .width_percent(86)
        .height_percent(84)
        .min_size(76, 28)
        .max_size(170, 58)
        .border_color(Color::Magenta)
        .focused_border_color(Color::LightMagenta)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| match stage {
            SideStage::Compose => draw_compose(
                frame,
                area,
                &question,
                models,
                focused,
                model_picker,
                effort_picker,
                clicks,
            ),
            SideStage::Transcript => draw_transcript(
                frame,
                area,
                snapshot,
                selected_history,
                focused,
                &mut answer_scroll,
                animation_frame,
                history_picker,
                clicks,
            ),
            SideStage::Closed => {}
        });
        popup.render(frame);
        self.answer_scroll = answer_scroll;
    }

    fn sync_models(&mut self, models: &[String]) {
        let selected = self
            .model_choices
            .get(self.model_picker.selected_index)
            .cloned();
        self.model_picker.set_total(models.len());
        if let Some(index) = selected
            .as_deref()
            .and_then(|selected| models.iter().position(|model| model == selected))
        {
            self.model_picker.select(index);
        }
        self.model_choices = models.to_vec();
    }
}

impl Default for SideChatUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_compose(
    frame: &mut Frame<'_>,
    area: Rect,
    question: &str,
    models: &[String],
    focused: Option<SideFocus>,
    model_picker: &mut ListPickerState,
    effort_picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<SideHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(8),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(text(Text::SideCommittedSnapshotHelp))
            .style(Style::default().fg(Color::LightMagenta))
            .wrap(Wrap { trim: true }),
        rows[0],
    );
    let question_block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(SideFocus::Question) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(
            " {} ({}/{}) ",
            text(Text::QuestionLabel),
            question.len(),
            crate::agent::side_chat::MAX_SIDE_QUESTION_BYTES
        ));
    let question_inner = question_block.inner(rows[1]);
    frame.render_widget(question_block, rows[1]);
    frame.render_widget(
        Paragraph::new(sanitize_for_display(question)).wrap(Wrap { trim: false }),
        question_inner,
    );
    clicks.register(rows[1], SideHit::Question);

    let pickers =
        Layout::horizontal([Constraint::Percentage(62), Constraint::Percentage(38)]).split(rows[2]);
    draw_picker(
        frame,
        pickers[0],
        text(Text::SeparateModelTitle),
        &models
            .iter()
            .map(|model| sanitize_for_display(model))
            .collect::<Vec<_>>(),
        model_picker,
        focused == Some(SideFocus::Model),
        SideHit::Model,
        clicks,
    );
    let efforts = [Text::Low, Text::Medium, Text::High, Text::XHigh, Text::Max]
        .into_iter()
        .map(|label| text(label).to_owned())
        .collect::<Vec<_>>();
    draw_picker(
        frame,
        pickers[1],
        text(Text::SideEffortTitle),
        &efforts,
        effort_picker,
        focused == Some(SideFocus::Effort),
        SideHit::Effort,
        clicks,
    );
    draw_buttons(
        frame,
        rows[3],
        focused,
        text(Text::AskWithoutTools),
        Some(text(Text::HistoryLabel)),
        has_visible_text(question),
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &SideChatSnapshot,
    selected: usize,
    focused: Option<SideFocus>,
    answer_scroll: &mut u16,
    animation_frame: usize,
    history_picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<SideHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["◐", "◓", "◑", "◒"][animation_frame % 4];
    frame.render_widget(
        Paragraph::new(format!(
            "{pulse} {} · {} {}",
            text(Text::SideChannelPinned),
            snapshot.exchanges.len(),
            text(Text::ExchangesLabel)
        ))
        .style(Style::default().fg(Color::LightMagenta)),
        rows[0],
    );
    let columns =
        Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)]).split(rows[1]);
    let labels = snapshot
        .exchanges
        .iter()
        .map(|exchange| {
            let marker = match exchange.status {
                SideExchangeStatus::Running => "…",
                SideExchangeStatus::Completed => "✓",
                SideExchangeStatus::Failed => "!",
                SideExchangeStatus::Cancelled => "×",
            };
            format!(
                "{marker} #{} {}",
                exchange.id,
                truncate_for_display(&sanitize_for_display(&exchange.question), 72)
            )
        })
        .collect::<Vec<_>>();
    draw_picker(
        frame,
        columns[0],
        text(Text::SideHistoryTitle),
        &labels,
        history_picker,
        focused == Some(SideFocus::History),
        SideHit::History,
        clicks,
    );
    draw_exchange(
        frame,
        columns[1],
        snapshot.exchanges.get(selected),
        answer_scroll,
    );
    let selected = snapshot.exchanges.get(selected);
    let (primary, secondary, enabled) = match selected.map(|exchange| exchange.status) {
        Some(SideExchangeStatus::Running) => (
            text(Text::CancelRequest),
            Some(text(Text::NewAfterFinish)),
            true,
        ),
        Some(SideExchangeStatus::Completed) => (
            text(Text::PromoteToComposer),
            Some(text(Text::NewQuestion)),
            true,
        ),
        Some(SideExchangeStatus::Failed | SideExchangeStatus::Cancelled) => {
            (text(Text::NewQuestion), None, true)
        }
        None => (text(Text::NewQuestion), None, true),
    };
    draw_buttons(frame, rows[2], focused, primary, secondary, enabled, clicks);
}

fn draw_exchange(
    frame: &mut Frame<'_>,
    area: Rect,
    exchange: Option<&SideExchange>,
    scroll: &mut u16,
) {
    let Some(exchange) = exchange else {
        frame.render_widget(
            Paragraph::new(text(Text::NoSideQuestionSelected)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(text(Text::DetailTitle)),
            ),
            area,
        );
        return;
    };
    let safe_question = sanitize_for_display(&exchange.question);
    let safe_answer = sanitize_for_display(&exchange.answer);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "#{} · {} · {} · {} {}",
                exchange.id,
                sanitize_for_display(&exchange.deployment),
                exchange.reasoning_effort,
                text(Text::ContextRevisionLabel),
                exchange.context_revision
            ),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            safe_question,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if exchange.status == SideExchangeStatus::Completed {
        if let Some(highlighted) = syntax::highlight_source("side-answer.md", &safe_answer) {
            lines.extend(highlighted.into_iter().map(Line::from));
        } else {
            lines.extend(safe_answer.lines().map(|line| Line::from(line.to_owned())));
        }
    } else {
        lines.push(Line::from(sanitize_for_display(&notice_text(
            &exchange.notice,
        ))));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "{} · {} {}/{}/{}",
        sanitize_for_display(&notice_text(&exchange.notice)),
        text(Text::TokensInOutTotal),
        exchange.input_tokens,
        exchange.output_tokens,
        exchange.total_tokens
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .title(text(Text::SanitizedAnswerProvisional));
    let inner = block.inner(area);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let max_scroll = paragraph
        .line_count(inner.width.max(1))
        .saturating_sub(usize::from(inner.height))
        .min(usize::from(u16::MAX)) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(paragraph.block(block).scroll((*scroll, 0)), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_picker<F>(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    labels: &[String],
    picker: &mut ListPickerState,
    focused: bool,
    hit: F,
    clicks: &mut ClickRegionRegistry<SideHit>,
) where
    F: Fn(usize) -> SideHit,
{
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::LightCyan
        } else {
            Color::Gray
        }))
        .title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    picker.set_total(labels.len());
    picker.ensure_visible(usize::from(inner.height));
    frame.render_widget(
        ListPicker::new(labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for row in 0..usize::from(inner.height) {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= labels.len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            hit(index),
        );
    }
}

fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<SideFocus>,
    primary: &str,
    secondary: Option<&str>,
    primary_enabled: bool,
    clicks: &mut ClickRegionRegistry<SideHit>,
) {
    let areas = Layout::horizontal([
        Constraint::Length(24),
        Constraint::Length(18),
        Constraint::Length(14),
        Constraint::Fill(1),
    ])
    .split(area);
    let mut primary_state = if primary_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    primary_state.set_focused(focused == Some(SideFocus::Primary));
    let primary_region = Button::new(primary, &primary_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(areas[0], frame.buffer_mut());
    if primary_enabled {
        clicks.register(primary_region.area, SideHit::Primary);
    }
    if let Some(label) = secondary {
        let mut secondary_state = ButtonState::enabled();
        secondary_state.set_focused(focused == Some(SideFocus::Secondary));
        let secondary_region = Button::new(label, &secondary_state)
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::default())
            .render_stateful(areas[1], frame.buffer_mut());
        clicks.register(secondary_region.area, SideHit::Secondary);
    }
    let mut close_state = ButtonState::enabled();
    close_state.set_focused(focused == Some(SideFocus::Close));
    let close_region = Button::new(text(Text::Close), &close_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(areas[2], frame.buffer_mut());
    clicks.register(close_region.area, SideHit::Close);
}

const fn effort_index(effort: ReasoningEffort) -> usize {
    match effort {
        ReasoningEffort::Low => 0,
        ReasoningEffort::Medium => 1,
        ReasoningEffort::High => 2,
        ReasoningEffort::XHigh => 3,
        ReasoningEffort::Max => 4,
    }
}

const fn effort_from_index(index: usize) -> ReasoningEffort {
    match index {
        0 => ReasoningEffort::Low,
        2 => ReasoningEffort::High,
        3 => ReasoningEffort::XHigh,
        4 => ReasoningEffort::Max,
        _ => ReasoningEffort::Medium,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn exchange(id: u64, deployment: &str) -> SideExchange {
        SideExchange {
            id,
            context_revision: 4,
            question: "Why?".to_owned(),
            answer: "Because `state` is immutable.".to_owned(),
            deployment: deployment.to_owned(),
            reasoning_effort: ReasoningEffort::Medium,
            status: SideExchangeStatus::Completed,
            notice: crate::notice::UiNotice::SideAnswerProvisional,
            created_at: Utc::now(),
            completed_at: Some(Utc::now()),
            input_tokens: 10,
            cached_input_tokens: 2,
            output_tokens: 5,
            total_tokens: 15,
        }
    }

    #[test]
    fn compose_and_transcript_controls_have_real_mouse_regions()
    -> Result<(), Box<dyn std::error::Error>> {
        let models = vec!["fast".to_owned(), "deep".to_owned()];
        let mut ui = SideChatUiState::new();
        ui.open(
            &SideChatSnapshot::default(),
            &models,
            "fast",
            ReasoningEffort::Medium,
        );
        ui.push_text("Why is this retry safe?");
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| ui.draw(frame, &SideChatSnapshot::default(), &models))?;
        assert!((0..40).any(|row| {
            (0..120).any(|column| ui.clicked(column, row) == Some(SideHit::Primary))
        }));

        let snapshot = SideChatSnapshot {
            revision: 1,
            exchanges: Arc::from([exchange(7, "fast")]),
        };
        ui.open(&snapshot, &models, "fast", ReasoningEffort::Medium);
        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &snapshot, &models))?;
        assert!((0..40).any(|row| {
            (0..120).any(|column| ui.clicked(column, row) == Some(SideHit::History(0)))
        }));
        assert!((0..40).any(|row| {
            (0..120).any(|column| ui.clicked(column, row) == Some(SideHit::Primary))
        }));
        Ok(())
    }

    #[test]
    fn question_backspace_removes_one_grapheme() {
        let mut ui = SideChatUiState::new();
        ui.push_text("👩‍💻");

        ui.pop_char();

        assert!(ui.question().is_empty());
    }

    #[test]
    fn model_selection_follows_the_model_when_choices_reorder()
    -> Result<(), Box<dyn std::error::Error>> {
        let models = vec!["fast".to_owned(), "deep".to_owned()];
        let reordered = vec!["deep".to_owned(), "fast".to_owned()];
        let mut ui = SideChatUiState::new();
        ui.open(
            &SideChatSnapshot::default(),
            &models,
            "fast",
            ReasoningEffort::Medium,
        );
        ui.select_model(1);
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;

        terminal.draw(|frame| ui.draw(frame, &SideChatSnapshot::default(), &reordered))?;

        assert_eq!(ui.selected_model_index(), 0);
        Ok(())
    }

    #[test]
    fn history_selection_follows_the_exchange_when_the_front_is_trimmed() {
        let first = SideChatSnapshot {
            revision: 1,
            exchanges: Arc::from([
                exchange(1, "fast"),
                exchange(2, "fast"),
                exchange(3, "fast"),
            ]),
        };
        let second = SideChatSnapshot {
            revision: 2,
            exchanges: Arc::from([
                exchange(2, "fast"),
                exchange(3, "fast"),
                exchange(4, "fast"),
            ]),
        };
        let mut ui = SideChatUiState::new();
        ui.open(
            &first,
            &["fast".to_owned()],
            "fast",
            ReasoningEffort::Medium,
        );
        ui.select_history(1);

        ui.sync(&second);

        assert_eq!(ui.selected_exchange(&second).map(|item| item.id), Some(2));
    }

    #[test]
    fn submitted_question_selects_the_new_exchange_when_history_is_full() {
        let first = SideChatSnapshot {
            revision: 1,
            exchanges: Arc::from((1..=32).map(|id| exchange(id, "fast")).collect::<Vec<_>>()),
        };
        let second = SideChatSnapshot {
            revision: 2,
            exchanges: Arc::from((2..=33).map(|id| exchange(id, "fast")).collect::<Vec<_>>()),
        };
        let mut ui = SideChatUiState::new();
        ui.open(
            &first,
            &["fast".to_owned()],
            "fast",
            ReasoningEffort::Medium,
        );
        ui.select_history(0);
        ui.compose();
        ui.mark_submitted();

        ui.sync(&second);

        assert_eq!(ui.selected_exchange(&second).map(|item| item.id), Some(33));
    }

    #[test]
    fn answer_scroll_is_clamped_to_rendered_content() -> Result<(), Box<dyn std::error::Error>> {
        let mut item = exchange(1, "fast");
        item.answer = "short".to_owned();
        let snapshot = SideChatSnapshot {
            revision: 1,
            exchanges: Arc::from([item]),
        };
        let mut ui = SideChatUiState::new();
        ui.open(
            &snapshot,
            &["fast".to_owned()],
            "fast",
            ReasoningEffort::Medium,
        );
        ui.scroll_answer(i16::MAX);
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;

        terminal.draw(|frame| ui.draw(frame, &snapshot, &["fast".to_owned()]))?;

        assert_eq!(ui.answer_scroll, 0);
        Ok(())
    }

    #[test]
    fn model_and_exchange_metadata_are_sanitized() -> Result<(), Box<dyn std::error::Error>> {
        let unsafe_model = "safe\u{202e}model".to_owned();
        let snapshot = SideChatSnapshot {
            revision: 1,
            exchanges: Arc::from([exchange(1, &unsafe_model)]),
        };
        let mut ui = SideChatUiState::new();
        ui.open(
            &SideChatSnapshot::default(),
            std::slice::from_ref(&unsafe_model),
            &unsafe_model,
            ReasoningEffort::Medium,
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| {
            ui.draw(
                frame,
                &SideChatSnapshot::default(),
                std::slice::from_ref(&unsafe_model),
            )
        })?;
        let compose = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!compose.contains('\u{202e}'));
        assert!(compose.contains("<U+202E>"));

        ui.open(
            &snapshot,
            std::slice::from_ref(&unsafe_model),
            &unsafe_model,
            ReasoningEffort::Medium,
        );
        terminal.draw(|frame| ui.draw(frame, &snapshot, std::slice::from_ref(&unsafe_model)))?;
        let transcript = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!transcript.contains('\u{202e}'));
        assert!(transcript.contains("<U+202E>"));
        Ok(())
    }
}
