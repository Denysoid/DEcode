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
use unicode_segmentation::UnicodeSegmentation as _;

use crate::agent::{
    FollowUpItem, FollowUpMode, FollowUpSnapshot, FollowUpStatus, side_chat::has_visible_text,
};

use super::{
    i18n::{Text, notice_text, text},
    render::{sanitize_for_display, truncate_for_display},
    syntax,
};

const ANIMATION_STEP: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUpStage {
    Closed,
    Compose,
    Browse,
    Edit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowUpFocus {
    Editor,
    Items,
    Primary,
    Secondary,
    Tertiary,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowUpHit {
    Editor,
    Item(usize),
    Queue,
    Steer,
    Browse,
    Edit,
    CancelOrRetry,
    RunNext,
    New,
    Save,
    Close,
}

#[derive(Debug, Clone)]
pub struct FollowUpUiState {
    stage: FollowUpStage,
    dialog: DialogState<()>,
    picker: ListPickerState,
    item_ids: Vec<u64>,
    selected_item_id: Option<u64>,
    focus: FocusManager<FollowUpFocus>,
    clicks: ClickRegionRegistry<FollowUpHit>,
    editor: String,
    editing: Option<(u64, u64)>,
    detail_scroll: u16,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl FollowUpUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            FollowUpFocus::Editor,
            FollowUpFocus::Items,
            FollowUpFocus::Primary,
            FollowUpFocus::Secondary,
            FollowUpFocus::Tertiary,
            FollowUpFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(FollowUpFocus::Editor);
        Self {
            stage: FollowUpStage::Closed,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            item_ids: Vec::new(),
            selected_item_id: None,
            focus,
            clicks: ClickRegionRegistry::new(),
            editor: String::new(),
            editing: None,
            detail_scroll: 0,
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, FollowUpStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> FollowUpStage {
        self.stage
    }

    #[must_use]
    pub fn focused(&self) -> Option<FollowUpFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn editor(&self) -> &str {
        &self.editor
    }

    #[must_use]
    pub const fn editing(&self) -> Option<(u64, u64)> {
        self.editing
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    pub fn open(&mut self, snapshot: &FollowUpSnapshot, compose: bool) {
        self.sync(snapshot);
        self.stage = if compose || snapshot.items.is_empty() {
            FollowUpStage::Compose
        } else {
            FollowUpStage::Browse
        };
        self.focus
            .set(if matches!(self.stage, FollowUpStage::Compose) {
                FollowUpFocus::Editor
            } else {
                FollowUpFocus::Items
            });
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = FollowUpStage::Closed;
        self.dialog.hide();
        self.clicks.clear();
        self.editing = None;
        self.detail_scroll = 0;
    }

    pub fn compose(&mut self) {
        self.stage = FollowUpStage::Compose;
        self.editor.clear();
        self.editing = None;
        self.focus.set(FollowUpFocus::Editor);
    }

    pub fn browse(&mut self) {
        self.stage = FollowUpStage::Browse;
        self.editing = None;
        self.detail_scroll = 0;
        self.focus.set(FollowUpFocus::Items);
    }

    pub fn begin_edit(&mut self, item: &FollowUpItem) {
        self.editor.clone_from(&item.text);
        self.editing = Some((item.id, item.revision));
        self.stage = FollowUpStage::Edit;
        self.focus.set(FollowUpFocus::Editor);
    }

    pub fn clear_after_submit(&mut self) {
        self.editor.clear();
        self.editing = None;
        self.stage = FollowUpStage::Browse;
        self.focus.set(FollowUpFocus::Items);
    }

    pub fn sync(&mut self, snapshot: &FollowUpSnapshot) {
        if let Some(id) = self.selected_item_id
            && let Some(index) = snapshot.items.iter().position(|item| item.id == id)
        {
            self.picker.select(index);
        }
        self.picker.set_total(snapshot.items.len());
        if !snapshot.items.is_empty() && self.picker.selected_index >= snapshot.items.len() {
            self.picker.select(snapshot.items.len().saturating_sub(1));
        }
        self.item_ids = snapshot.items.iter().map(|item| item.id).collect();
        self.selected_item_id = snapshot
            .items
            .get(self.picker.selected_index)
            .map(|item| item.id);
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
        let next = match (self.stage, self.focused()) {
            (FollowUpStage::Compose, Some(FollowUpFocus::Editor)) => FollowUpFocus::Primary,
            (FollowUpStage::Compose, Some(FollowUpFocus::Primary)) => FollowUpFocus::Secondary,
            (FollowUpStage::Compose, Some(FollowUpFocus::Secondary)) => FollowUpFocus::Tertiary,
            (FollowUpStage::Compose, Some(FollowUpFocus::Tertiary)) => FollowUpFocus::Close,
            (FollowUpStage::Compose, _) => FollowUpFocus::Editor,
            (FollowUpStage::Browse, Some(FollowUpFocus::Items)) => FollowUpFocus::Primary,
            (FollowUpStage::Browse, Some(FollowUpFocus::Primary)) => FollowUpFocus::Secondary,
            (FollowUpStage::Browse, Some(FollowUpFocus::Secondary)) => FollowUpFocus::Tertiary,
            (FollowUpStage::Browse, Some(FollowUpFocus::Tertiary)) => FollowUpFocus::Close,
            (FollowUpStage::Browse, _) => FollowUpFocus::Items,
            (FollowUpStage::Edit, Some(FollowUpFocus::Editor)) => FollowUpFocus::Primary,
            (FollowUpStage::Edit, Some(FollowUpFocus::Primary)) => FollowUpFocus::Close,
            (FollowUpStage::Edit, _) => FollowUpFocus::Editor,
            (FollowUpStage::Closed, _) => FollowUpFocus::Close,
        };
        self.focus.set(next);
    }

    pub fn previous_focus(&mut self) {
        let previous = match (self.stage, self.focused()) {
            (FollowUpStage::Compose, Some(FollowUpFocus::Editor)) => FollowUpFocus::Close,
            (FollowUpStage::Compose, Some(FollowUpFocus::Primary)) => FollowUpFocus::Editor,
            (FollowUpStage::Compose, Some(FollowUpFocus::Secondary)) => FollowUpFocus::Primary,
            (FollowUpStage::Compose, Some(FollowUpFocus::Tertiary)) => FollowUpFocus::Secondary,
            (FollowUpStage::Compose, _) => FollowUpFocus::Tertiary,
            (FollowUpStage::Browse, Some(FollowUpFocus::Items)) => FollowUpFocus::Close,
            (FollowUpStage::Browse, Some(FollowUpFocus::Primary)) => FollowUpFocus::Items,
            (FollowUpStage::Browse, Some(FollowUpFocus::Secondary)) => FollowUpFocus::Primary,
            (FollowUpStage::Browse, Some(FollowUpFocus::Tertiary)) => FollowUpFocus::Secondary,
            (FollowUpStage::Browse, _) => FollowUpFocus::Tertiary,
            (FollowUpStage::Edit, Some(FollowUpFocus::Editor)) => FollowUpFocus::Close,
            (FollowUpStage::Edit, Some(FollowUpFocus::Primary)) => FollowUpFocus::Editor,
            (FollowUpStage::Edit, _) => FollowUpFocus::Primary,
            (FollowUpStage::Closed, _) => FollowUpFocus::Close,
        };
        self.focus.set(previous);
    }

    pub fn focus(&mut self, focus: FollowUpFocus) {
        self.focus.set(focus);
    }

    pub fn next_item(&mut self) {
        if self.focused() == Some(FollowUpFocus::Items) {
            self.picker.select_next();
            self.update_selected_item_id();
            self.detail_scroll = 0;
        }
    }

    pub fn previous_item(&mut self) {
        if self.focused() == Some(FollowUpFocus::Items) {
            self.picker.select_prev();
            self.update_selected_item_id();
            self.detail_scroll = 0;
        }
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.update_selected_item_id();
        self.detail_scroll = 0;
        self.focus.set(FollowUpFocus::Items);
    }

    pub fn scroll_detail(&mut self, delta: i16) {
        if delta.is_negative() {
            self.detail_scroll = self.detail_scroll.saturating_sub(delta.unsigned_abs());
        } else {
            self.detail_scroll = self.detail_scroll.saturating_add(delta.unsigned_abs());
        }
    }

    pub fn push_char(&mut self, character: char) {
        if (character == '\n' || character == '\t' || !character.is_control())
            && self.editor.len().saturating_add(character.len_utf8())
                <= crate::agent::followups::MAX_FOLLOW_UP_BYTES
        {
            self.editor.push(character);
        }
    }

    pub fn push_text(&mut self, text: &str) {
        for character in text.chars() {
            if character != '\n' && character != '\t' && character.is_control() {
                continue;
            }
            if self.editor.len().saturating_add(character.len_utf8())
                > crate::agent::followups::MAX_FOLLOW_UP_BYTES
            {
                break;
            }
            self.editor.push(character);
        }
    }

    pub fn pop_char(&mut self) {
        if let Some((start, _)) = self.editor.grapheme_indices(true).next_back() {
            self.editor.truncate(start);
        }
    }

    #[must_use]
    pub fn editor_has_visible_text(&self) -> bool {
        has_visible_text(&self.editor)
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<FollowUpHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        snapshot: &FollowUpSnapshot,
        busy: bool,
        can_dispatch: bool,
    ) {
        if !self.is_open() {
            return;
        }
        self.sync(snapshot);
        let stage = self.stage;
        let focused = self.focused();
        let selected = self.picker.selected_index;
        let scroll = self.detail_scroll;
        let animation = self.animation_frame;
        let editor = self.editor.clone();
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::QueueFollowUps))
            .width_percent(88)
            .height_percent(84)
            .min_size(78, 28)
            .max_size(172, 58)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| match stage {
            FollowUpStage::Compose | FollowUpStage::Edit => {
                draw_editor(frame, area, stage, &editor, focused, busy, clicks)
            }
            FollowUpStage::Browse => draw_browser(
                frame,
                area,
                snapshot,
                selected,
                focused,
                scroll,
                animation,
                busy,
                can_dispatch,
                picker,
                clicks,
            ),
            FollowUpStage::Closed => {}
        });
        popup.render(frame);
    }

    fn update_selected_item_id(&mut self) {
        self.selected_item_id = self.item_ids.get(self.picker.selected_index).copied();
    }
}

impl Default for FollowUpUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_editor(
    frame: &mut Frame<'_>,
    area: Rect,
    stage: FollowUpStage,
    editor: &str,
    focused: Option<FollowUpFocus>,
    busy: bool,
    clicks: &mut ClickRegionRegistry<FollowUpHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(12),
        Constraint::Length(3),
    ])
    .split(area);
    let help = if matches!(stage, FollowUpStage::Edit) {
        text(Text::FollowUpEditRevisionHelp)
    } else {
        text(Text::QueueSteerHelp)
    };
    frame.render_widget(
        Paragraph::new(help)
            .style(Style::default().fg(Color::LightCyan))
            .wrap(Wrap { trim: true }),
        rows[0],
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default().fg(if focused == Some(FollowUpFocus::Editor) {
                Color::LightCyan
            } else {
                Color::Gray
            }),
        )
        .title(format!(
            " {} ({}/{}) ",
            text(Text::FollowUpLabel),
            editor.len(),
            crate::agent::followups::MAX_FOLLOW_UP_BYTES
        ));
    let inner = block.inner(rows[1]);
    frame.render_widget(block, rows[1]);
    frame.render_widget(
        Paragraph::new(sanitize_for_display(editor)).wrap(Wrap { trim: false }),
        inner,
    );
    clicks.register(rows[1], FollowUpHit::Editor);
    let can_submit = has_visible_text(editor);
    if matches!(stage, FollowUpStage::Edit) {
        draw_button_row(
            frame,
            rows[2],
            focused,
            &[(text(Text::SaveEdit), FollowUpHit::Save, can_submit)],
            clicks,
        );
    } else {
        draw_button_row(
            frame,
            rows[2],
            focused,
            &[
                (text(Text::QueueLabel), FollowUpHit::Queue, can_submit),
                (
                    text(Text::SteerSafely),
                    FollowUpHit::Steer,
                    busy && can_submit,
                ),
                (text(Text::BrowseLabel), FollowUpHit::Browse, true),
            ],
            clicks,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_browser(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &FollowUpSnapshot,
    selected: usize,
    focused: Option<FollowUpFocus>,
    scroll: u16,
    animation: usize,
    busy: bool,
    can_dispatch: bool,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<FollowUpHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(15),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["◐", "◓", "◑", "◒"][animation % 4];
    frame.render_widget(
        Paragraph::new(format!(
            "{pulse} {} {}",
            snapshot.pending_count(),
            text(Text::PendingStrictFifo)
        ))
        .style(Style::default().fg(Color::LightCyan)),
        rows[0],
    );
    let columns =
        Layout::horizontal([Constraint::Percentage(37), Constraint::Percentage(63)]).split(rows[1]);
    let labels = snapshot
        .items
        .iter()
        .map(|item| {
            format!(
                "{} {} #{} r{} {}",
                status_marker(item.status),
                item.mode,
                item.id,
                item.revision,
                truncate_for_display(&sanitize_for_display(&item.text), 52)
            )
        })
        .collect::<Vec<_>>();
    draw_picker(
        frame,
        columns[0],
        &labels,
        picker,
        focused == Some(FollowUpFocus::Items),
        clicks,
    );
    let item = snapshot.items.get(selected);
    draw_detail(frame, columns[1], item, scroll);
    let buttons = item.map_or_else(
        || {
            vec![
                (text(Text::New), FollowUpHit::New, true),
                (text(Text::NoAction), FollowUpHit::CancelOrRetry, false),
                (text(Text::QueueEmpty), FollowUpHit::RunNext, false),
            ]
        },
        |item| match item.status {
            FollowUpStatus::Pending => vec![
                (text(Text::EditLabel), FollowUpHit::Edit, true),
                (text(Text::Cancel), FollowUpHit::CancelOrRetry, true),
                (
                    text(Text::RunFifoNext),
                    FollowUpHit::RunNext,
                    can_dispatch && item.mode == FollowUpMode::Queue,
                ),
            ],
            FollowUpStatus::Failed => vec![
                (text(Text::EditLabel), FollowUpHit::Edit, true),
                (
                    text(Text::Retry),
                    FollowUpHit::CancelOrRetry,
                    item.mode == FollowUpMode::Queue || busy,
                ),
                (text(Text::New), FollowUpHit::New, true),
            ],
            FollowUpStatus::Dispatching | FollowUpStatus::Delivered | FollowUpStatus::Cancelled => {
                vec![
                    (text(Text::New), FollowUpHit::New, true),
                    (text(Text::NoAction), FollowUpHit::CancelOrRetry, false),
                    (text(Text::NoAction), FollowUpHit::RunNext, false),
                ]
            }
        },
    );
    draw_button_row(frame, rows[2], focused, &buttons, clicks);
}

fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    labels: &[String],
    picker: &mut ListPickerState,
    focused: bool,
    clicks: &mut ClickRegionRegistry<FollowUpHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::LightCyan
        } else {
            Color::Gray
        }))
        .title(format!(" {} ", text(Text::DurableItems)));
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
            FollowUpHit::Item(index),
        );
    }
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, item: Option<&FollowUpItem>, scroll: u16) {
    let Some(item) = item else {
        frame.render_widget(
            Paragraph::new(text(Text::NoFollowUpSelected)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::DetailTitle))),
            ),
            area,
        );
        return;
    };
    let safe_text = sanitize_for_display(&item.text);
    let mut lines = vec![
        Line::from(Span::styled(
            format!(
                "{} #{} · {} {} · {:?}",
                item.mode,
                item.id,
                text(Text::Revision),
                item.revision,
                item.status
            ),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{}: {} · {}: {}",
            text(Text::TargetTurnLabel),
            item.target_turn_id
                .map_or_else(|| text(Text::NextLower).to_owned(), |turn| turn.to_string()),
            text(Text::ManualResumeLabel),
            if item.requires_manual_dispatch {
                text(Text::YesLabel)
            } else {
                text(Text::NoLabel)
            }
        )),
        Line::from(sanitize_for_display(&notice_text(&item.notice))),
        Line::from(""),
    ];
    if let Some(highlighted) = syntax::highlight_source("follow-up.md", &safe_text) {
        lines.extend(highlighted.into_iter().map(Line::from));
    } else {
        lines.extend(safe_text.lines().map(|line| Line::from(line.to_owned())));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::SanitizedFollowUp))),
            )
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_button_row(
    frame: &mut Frame<'_>,
    area: Rect,
    focused: Option<FollowUpFocus>,
    buttons: &[(&str, FollowUpHit, bool)],
    clicks: &mut ClickRegionRegistry<FollowUpHit>,
) {
    let mut constraints = buttons
        .iter()
        .map(|_| Constraint::Length(19))
        .collect::<Vec<_>>();
    constraints.push(Constraint::Length(12));
    constraints.push(Constraint::Fill(1));
    let areas = Layout::horizontal(constraints).split(area);
    for (index, (label, hit, enabled)) in buttons.iter().enumerate() {
        let focus = match index {
            0 => FollowUpFocus::Primary,
            1 => FollowUpFocus::Secondary,
            _ => FollowUpFocus::Tertiary,
        };
        let mut state = if *enabled {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        state.set_focused(focused == Some(focus));
        let region = Button::new(label, &state)
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::default())
            .render_stateful(areas[index], frame.buffer_mut());
        if *enabled {
            clicks.register(region.area, *hit);
        }
    }
    let close_index = buttons.len();
    let mut close = ButtonState::enabled();
    close.set_focused(focused == Some(FollowUpFocus::Close));
    let region = Button::new(text(Text::Close), &close)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(areas[close_index], frame.buffer_mut());
    clicks.register(region.area, FollowUpHit::Close);
}

const fn status_marker(status: FollowUpStatus) -> &'static str {
    match status {
        FollowUpStatus::Pending => "○",
        FollowUpStatus::Dispatching => "◉",
        FollowUpStatus::Delivered => "✓",
        FollowUpStatus::Cancelled => "×",
        FollowUpStatus::Failed => "!",
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::agent::followups::FollowUpState;

    #[test]
    fn queue_steer_and_item_actions_have_real_mouse_regions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = FollowUpState::default();
        state.enqueue(FollowUpMode::Queue, "run later".to_owned(), None)?;
        let snapshot = state.snapshot();
        let mut ui = FollowUpUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        ui.open(&FollowUpSnapshot::default(), true);
        ui.push_text("pivot safely");
        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &FollowUpSnapshot::default(), true, false))?;
        assert!(has_hit(&ui, FollowUpHit::Queue));
        assert!(has_hit(&ui, FollowUpHit::Steer));

        ui.open(&snapshot, false);
        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &snapshot, false, true))?;
        assert!(has_hit(&ui, FollowUpHit::Item(0)));
        assert!(has_hit(&ui, FollowUpHit::Edit));
        assert!(has_hit(&ui, FollowUpHit::CancelOrRetry));
        assert!(has_hit(&ui, FollowUpHit::RunNext));
        Ok(())
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let mut ui = FollowUpUiState::new();
        ui.push_text("e\u{301}");

        ui.pop_char();

        assert!(ui.editor().is_empty());
    }

    #[test]
    fn paste_stops_at_the_first_character_that_does_not_fit() {
        let mut ui = FollowUpUiState::new();
        ui.push_text(&"a".repeat(crate::agent::followups::MAX_FOLLOW_UP_BYTES - 1));

        ui.push_text("éx");

        assert_eq!(
            ui.editor().len(),
            crate::agent::followups::MAX_FOLLOW_UP_BYTES - 1
        );
        assert!(ui.editor().ends_with('a'));
    }

    #[test]
    fn invisible_editor_text_does_not_enable_submit() -> Result<(), Box<dyn std::error::Error>> {
        let mut ui = FollowUpUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        ui.open(&FollowUpSnapshot::default(), true);
        ui.push_text("\u{200b}\u{2060}");

        ui.begin_frame();
        terminal.draw(|frame| ui.draw(frame, &FollowUpSnapshot::default(), true, false))?;

        assert!(!has_hit(&ui, FollowUpHit::Queue));
        assert!(!has_hit(&ui, FollowUpHit::Steer));
        Ok(())
    }

    #[test]
    fn selection_follows_the_item_when_an_earlier_row_is_pruned()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state = FollowUpState::default();
        let first = state.enqueue(FollowUpMode::Queue, "first".to_owned(), None)?;
        for index in 1..crate::agent::followups::MAX_FOLLOW_UP_ITEMS {
            state.enqueue(FollowUpMode::Queue, format!("item {index}"), None)?;
        }
        let snapshot = state.snapshot();
        let selected_id = snapshot.items[1].id;
        let mut ui = FollowUpUiState::new();
        ui.open(&snapshot, false);
        ui.select(1);

        state.cancel(first.id, first.revision)?;
        state.enqueue(FollowUpMode::Queue, "replacement".to_owned(), None)?;
        let snapshot = state.snapshot();
        ui.sync(&snapshot);

        assert_eq!(snapshot.items[ui.selected_index()].id, selected_id);
        Ok(())
    }

    fn has_hit(ui: &FollowUpUiState, expected: FollowUpHit) -> bool {
        (0..40).any(|row| (0..120).any(|column| ui.clicked(column, row) == Some(expected)))
    }
}
