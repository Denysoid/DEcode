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
        DialogConfig, DialogState, ListPicker, ListPickerState, ListPickerStyle, PopupDialog,
    },
    state::FocusManager,
};
use unicode_width::UnicodeWidthStr;

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
};

const MAX_NOTIFICATIONS: usize = 128;
const MAX_TITLE_GRAPHEMES: usize = 160;
const MAX_BODY_GRAPHEMES: usize = 2_000;
const ANIMATION_STEP: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    NeedsAction,
    Completed,
    Error,
}

impl NotificationKind {
    fn label(self) -> &'static str {
        match self {
            Self::NeedsAction => text(Text::ActionNotificationLabel),
            Self::Completed => text(Text::DoneNotificationLabel),
            Self::Error => text(Text::ErrorNotificationLabel),
        }
    }

    const fn color(self) -> Color {
        match self {
            Self::NeedsAction => Color::Yellow,
            Self::Completed => Color::Green,
            Self::Error => Color::Red,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotificationPreferences {
    pub bell_on_action: bool,
    pub bell_on_completion: bool,
    pub bell_on_error: bool,
}

impl NotificationPreferences {
    const fn rings_for(self, kind: NotificationKind) -> bool {
        match kind {
            NotificationKind::NeedsAction => self.bell_on_action,
            NotificationKind::Completed => self.bell_on_completion,
            NotificationKind::Error => self.bell_on_error,
        }
    }
}

impl Default for NotificationPreferences {
    fn default() -> Self {
        Self {
            bell_on_action: true,
            bell_on_completion: true,
            bell_on_error: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u64,
    pub kind: NotificationKind,
    pub title: String,
    pub body: String,
    pub read: bool,
    created_at: Instant,
    causal_key: String,
}

#[derive(Debug, Clone)]
pub struct NotificationCenter {
    items: Vec<Notification>,
    next_id: u64,
    preferences: NotificationPreferences,
    bell_pending: bool,
}

impl NotificationCenter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
            preferences: NotificationPreferences::default(),
            bell_pending: false,
        }
    }

    #[must_use]
    pub fn items(&self) -> &[Notification] {
        &self.items
    }

    #[must_use]
    pub fn unread_count(&self) -> usize {
        self.items.iter().filter(|item| !item.read).count()
    }

    #[must_use]
    pub const fn preferences(&self) -> NotificationPreferences {
        self.preferences
    }

    pub fn toggle_action_bell(&mut self) {
        self.preferences.bell_on_action = !self.preferences.bell_on_action;
    }

    pub fn toggle_completion_bell(&mut self) {
        self.preferences.bell_on_completion = !self.preferences.bell_on_completion;
    }

    pub fn toggle_error_bell(&mut self) {
        self.preferences.bell_on_error = !self.preferences.bell_on_error;
    }

    pub fn push_unique(
        &mut self,
        causal_key: String,
        kind: NotificationKind,
        title: impl Into<String>,
        body: impl Into<String>,
    ) {
        if self.items.iter().any(|item| item.causal_key == causal_key) {
            return;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let title = title.into();
        let body = body.into();
        self.items.insert(
            0,
            Notification {
                id,
                kind,
                title: truncate_for_display(&sanitize_for_display(&title), MAX_TITLE_GRAPHEMES),
                body: truncate_for_display(&sanitize_for_display(&body), MAX_BODY_GRAPHEMES),
                read: false,
                created_at: Instant::now(),
                causal_key,
            },
        );
        self.items.truncate(MAX_NOTIFICATIONS);
        if self.preferences.rings_for(kind) {
            self.bell_pending = true;
        }
    }

    pub fn resolve(&mut self, causal_key: &str) {
        if let Some(item) = self
            .items
            .iter_mut()
            .find(|item| item.causal_key == causal_key)
        {
            item.read = true;
        }
    }

    pub fn mark_read(&mut self, index: usize) {
        if let Some(item) = self.items.get_mut(index) {
            item.read = true;
        }
    }

    pub fn mark_all_read(&mut self) {
        for item in &mut self.items {
            item.read = true;
        }
    }

    pub fn clear_read(&mut self) {
        self.items.retain(|item| !item.read);
    }

    #[must_use]
    pub fn take_bell_pending(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending)
    }
}

impl Default for NotificationCenter {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationFocus {
    Items,
    ActionBell,
    CompletionBell,
    ErrorBell,
    MarkAllRead,
    ClearRead,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationHit {
    Item(usize),
    ActionBell,
    CompletionBell,
    ErrorBell,
    MarkAllRead,
    ClearRead,
    Close,
}

#[derive(Debug, Clone)]
pub struct NotificationUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    item_ids: Vec<u64>,
    focus: FocusManager<NotificationFocus>,
    clicks: ClickRegionRegistry<NotificationHit>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl NotificationUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            NotificationFocus::Items,
            NotificationFocus::ActionBell,
            NotificationFocus::CompletionBell,
            NotificationFocus::ErrorBell,
            NotificationFocus::MarkAllRead,
            NotificationFocus::ClearRead,
            NotificationFocus::Close,
        ] {
            focus.register(item);
        }
        focus.set(NotificationFocus::Items);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            item_ids: Vec::new(),
            focus,
            clicks: ClickRegionRegistry::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, center: &NotificationCenter) {
        self.open = true;
        self.sync(center);
        self.focus.set(NotificationFocus::Items);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn sync(&mut self, center: &NotificationCenter) {
        let selected_id = self.item_ids.get(self.picker.selected_index).copied();
        let item_ids = center
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        self.picker.set_total(center.items().len());
        if let Some(index) =
            selected_id.and_then(|selected_id| item_ids.iter().position(|id| *id == selected_id))
        {
            self.picker.select(index);
        } else if !center.items().is_empty() && self.picker.selected_index >= center.items().len() {
            self.picker.select(center.items().len().saturating_sub(1));
        }
        self.item_ids = item_ids;
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

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.picker.selected_index
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.focus.set(NotificationFocus::Items);
    }

    pub fn next_item(&mut self) {
        self.picker.select_next();
    }

    pub fn previous_item(&mut self) {
        self.picker.select_prev();
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    #[must_use]
    pub fn focused(&self) -> Option<NotificationFocus> {
        self.focus.current().copied()
    }

    pub fn focus(&mut self, focus: NotificationFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<NotificationHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, center: &NotificationCenter) {
        if !self.open {
            return;
        }
        self.sync(center);
        let selected = self.picker.selected_index;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::Notifications))
            .width_percent(78)
            .height_percent(78)
            .min_size(72, 25)
            .max_size(138, 52)
            .border_color(Color::Magenta)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_notifications(
                frame,
                area,
                center,
                selected,
                focused,
                animation_frame,
                picker,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for NotificationUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_notifications(
    frame: &mut Frame<'_>,
    area: Rect,
    center: &NotificationCenter,
    selected: usize,
    focused: Option<NotificationFocus>,
    animation_frame: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<NotificationHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Min(6),
        Constraint::Length(7),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["( )", "(o)", "(*)", "(o)"][animation_frame % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!(
                    "{pulse} {} {} · {} {}",
                    center.unread_count(),
                    text(Text::UnreadLabel),
                    center.items().len(),
                    text(Text::RetainedLabel)
                ),
                Style::default()
                    .fg(if center.unread_count() == 0 {
                        Color::DarkGray
                    } else {
                        Color::LightMagenta
                    })
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::NotificationSnapshotHelp)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let preferences = center.preferences();
    let preference_areas = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .split(rows[1]);
    render_toggle(
        frame,
        preference_areas[0],
        text(Text::BellAction),
        preferences.bell_on_action,
        focused == Some(NotificationFocus::ActionBell),
        NotificationHit::ActionBell,
        clicks,
    );
    render_toggle(
        frame,
        preference_areas[1],
        text(Text::BellDone),
        preferences.bell_on_completion,
        focused == Some(NotificationFocus::CompletionBell),
        NotificationHit::CompletionBell,
        clicks,
    );
    render_toggle(
        frame,
        preference_areas[2],
        text(Text::BellErrors),
        preferences.bell_on_error,
        focused == Some(NotificationFocus::ErrorBell),
        NotificationHit::ErrorBell,
        clicks,
    );

    draw_items(frame, rows[2], center, focused, picker, clicks);
    draw_detail(frame, rows[3], center.items().get(selected));

    let labels = [
        text(Text::MarkAllRead),
        text(Text::ClearRead),
        text(Text::CloseEsc),
    ];
    let widths = notification_action_widths(labels);
    let button_areas = Layout::horizontal([
        Constraint::Length(widths[0]),
        Constraint::Length(widths[1]),
        Constraint::Length(widths[2]),
        Constraint::Fill(1),
    ])
    .spacing(1)
    .split(rows[4]);
    render_button(
        frame,
        button_areas[0],
        labels[0],
        focused == Some(NotificationFocus::MarkAllRead),
        NotificationHit::MarkAllRead,
        clicks,
    );
    render_button(
        frame,
        button_areas[1],
        labels[1],
        focused == Some(NotificationFocus::ClearRead),
        NotificationHit::ClearRead,
        clicks,
    );
    render_button(
        frame,
        button_areas[2],
        labels[2],
        focused == Some(NotificationFocus::Close),
        NotificationHit::Close,
        clicks,
    );
}

fn notification_action_widths(labels: [&str; 3]) -> [u16; 3] {
    labels.map(|label| {
        u16::try_from(UnicodeWidthStr::width(label).saturating_add(2)).unwrap_or(u16::MAX)
    })
}

fn draw_items(
    frame: &mut Frame<'_>,
    area: Rect,
    center: &NotificationCenter,
    focused: Option<NotificationFocus>,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<NotificationHit>,
) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused == Some(NotificationFocus::Items) {
            Style::default().fg(Color::LightCyan)
        } else {
            Style::default().fg(Color::Gray)
        })
        .title(format!(" {} ", text(Text::EventInboxTitle)));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let labels = center
        .items()
        .iter()
        .map(|item| {
            let unread = if item.read { " " } else { "●" };
            truncate_for_display(
                &sanitize_for_display(&format!("{unread} [{}] {}", item.kind.label(), item.title)),
                MAX_TITLE_GRAPHEMES,
            )
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
        if index >= center.items().len() {
            break;
        }
        clicks.register(
            Rect::new(inner.x, inner.y.saturating_add(row as u16), inner.width, 1),
            NotificationHit::Item(index),
        );
    }
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, item: Option<&Notification>) {
    let content = item.map_or_else(
        || vec![Line::from(text(Text::NoNotifications))],
        |item| {
            let age = Instant::now().saturating_duration_since(item.created_at);
            vec![
                Line::from(vec![
                    Span::styled(
                        item.kind.label(),
                        Style::default()
                            .fg(item.kind.color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " · #{} · {} {}",
                        item.id,
                        age.as_secs(),
                        text(Text::SecondsAgo)
                    )),
                ]),
                Line::from(Span::styled(
                    truncate_for_display(&sanitize_for_display(&item.title), MAX_TITLE_GRAPHEMES),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(truncate_for_display(
                    &sanitize_for_display(&item.body),
                    MAX_BODY_GRAPHEMES,
                )),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(content)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::SelectedEvent))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_toggle(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    checked: bool,
    focused: bool,
    hit: NotificationHit,
    clicks: &mut ClickRegionRegistry<NotificationHit>,
) {
    let mut state = CheckBoxState::new(checked);
    state.set_focused(focused);
    let region = CheckBox::new(label, &state)
        .style(
            CheckBoxStyle::custom(text(Text::OnLabel), text(Text::OffLabel))
                .checked_fg(Color::Green)
                .focused_fg(Color::LightCyan),
        )
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, hit);
}

fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    hit: NotificationHit,
    clicks: &mut ClickRegionRegistry<NotificationHit>,
) {
    let mut state = ButtonState::enabled();
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default())
        .render_stateful(area, frame.buffer_mut());
    clicks.register(region.area, hit);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};
    use unicode_width::UnicodeWidthStr;

    use super::*;
    use crate::{config::UiLanguage, ui::i18n::text_for};

    #[test]
    fn notification_action_widths_fit_localized_labels() {
        for language in UiLanguage::ALL {
            let labels = [
                text_for(language, Text::MarkAllRead),
                text_for(language, Text::ClearRead),
                text_for(language, Text::CloseEsc),
            ];
            let widths = notification_action_widths(labels);

            for (label, width) in labels.into_iter().zip(widths) {
                assert!(usize::from(width) >= UnicodeWidthStr::width(label) + 2);
            }
            assert!(
                widths.into_iter().sum::<u16>() + 2 <= 72,
                "labels do not fit for {language:?}"
            );
        }
    }

    #[test]
    fn ukrainian_notification_actions_render_without_clipping()
    -> Result<(), Box<dyn std::error::Error>> {
        let labels = ["Прочитати все", "Очистити прочитані", "Закрити (Esc)"];
        let widths = notification_action_widths(labels);
        let areas = Layout::horizontal([
            Constraint::Length(widths[0]),
            Constraint::Length(widths[1]),
            Constraint::Length(widths[2]),
            Constraint::Fill(1),
        ])
        .spacing(1)
        .split(Rect::new(0, 0, 72, 3));
        let mut terminal = Terminal::new(TestBackend::new(72, 3))?;
        let mut clicks = ClickRegionRegistry::new();
        terminal.draw(|frame| {
            render_button(
                frame,
                areas[0],
                labels[0],
                false,
                NotificationHit::MarkAllRead,
                &mut clicks,
            );
            render_button(
                frame,
                areas[1],
                labels[1],
                false,
                NotificationHit::ClearRead,
                &mut clicks,
            );
            render_button(
                frame,
                areas[2],
                labels[2],
                false,
                NotificationHit::Close,
                &mut clicks,
            );
        })?;
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        for label in labels {
            assert!(rendered.contains(label));
        }
        Ok(())
    }

    #[test]
    fn causal_events_are_deduplicated_bounded_and_ring_once() {
        let mut center = NotificationCenter::new();
        center.push_unique(
            "approval:1".to_owned(),
            NotificationKind::NeedsAction,
            "Approval needed",
            "Inspect the command",
        );
        center.push_unique(
            "approval:1".to_owned(),
            NotificationKind::NeedsAction,
            "Duplicate",
            "Must not be inserted",
        );
        assert_eq!(center.items().len(), 1);
        assert_eq!(center.unread_count(), 1);
        assert!(center.take_bell_pending());
        assert!(!center.take_bell_pending());

        center.resolve("approval:1");
        assert_eq!(center.unread_count(), 0);
        for index in 0..(MAX_NOTIFICATIONS + 10) {
            center.push_unique(
                format!("done:{index}"),
                NotificationKind::Completed,
                "Done",
                "Complete",
            );
        }
        assert_eq!(center.items().len(), MAX_NOTIFICATIONS);
    }

    #[test]
    fn popup_has_real_mouse_regions_and_sanitizes_untrusted_detail()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut center = NotificationCenter::new();
        center.push_unique(
            "error:1".to_owned(),
            NotificationKind::Error,
            "bad\u{1b}[31m title",
            "unsafe \u{202e} body",
        );
        let mut ui = NotificationUiState::new();
        ui.open(&center);
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;
        terminal.draw(|frame| ui.draw(frame, &center))?;
        for expected in [
            NotificationHit::Item(0),
            NotificationHit::ActionBell,
            NotificationHit::CompletionBell,
            NotificationHit::ErrorBell,
            NotificationHit::MarkAllRead,
            NotificationHit::ClearRead,
            NotificationHit::Close,
        ] {
            assert!(
                (0..36).any(|row| {
                    (0..110).any(|column| ui.clicked(column, row) == Some(expected))
                })
            );
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
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_reach_every_notification_control() {
        let mut ui = NotificationUiState::new();
        for expected in [
            NotificationFocus::Items,
            NotificationFocus::ActionBell,
            NotificationFocus::CompletionBell,
            NotificationFocus::ErrorBell,
            NotificationFocus::MarkAllRead,
            NotificationFocus::ClearRead,
            NotificationFocus::Close,
        ] {
            assert_eq!(ui.focused(), Some(expected));
            ui.next_focus();
        }
        assert_eq!(ui.focused(), Some(NotificationFocus::Items));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(NotificationFocus::Close));
    }

    #[test]
    fn selection_follows_notification_identity_when_new_items_arrive() {
        let mut center = NotificationCenter::new();
        center.push_unique(
            "first".to_owned(),
            NotificationKind::Completed,
            "first",
            "body",
        );
        center.push_unique(
            "second".to_owned(),
            NotificationKind::Completed,
            "second",
            "body",
        );
        let mut ui = NotificationUiState::new();
        ui.open(&center);
        ui.select(1);

        center.push_unique(
            "third".to_owned(),
            NotificationKind::Completed,
            "third",
            "body",
        );
        ui.sync(&center);

        assert_eq!(center.items()[ui.selected()].title, "first");
    }

    #[test]
    fn snapshot_change_invalidates_old_notification_hit_regions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut center = NotificationCenter::new();
        center.push_unique(
            "first".to_owned(),
            NotificationKind::Completed,
            "first",
            "body",
        );
        let mut ui = NotificationUiState::new();
        ui.open(&center);
        let mut terminal = Terminal::new(TestBackend::new(110, 36))?;
        terminal.draw(|frame| ui.draw(frame, &center))?;
        let old_point = (0..36).find_map(|row| {
            (0..110)
                .find(|column| ui.clicked(*column, row) == Some(NotificationHit::Item(0)))
                .map(|column| (column, row))
        });
        let (column, row) = old_point.ok_or("missing notification hit region")?;

        center.push_unique(
            "second".to_owned(),
            NotificationKind::Completed,
            "second",
            "body",
        );
        ui.sync(&center);

        assert_eq!(ui.clicked(column, row), None);
        Ok(())
    }
}
