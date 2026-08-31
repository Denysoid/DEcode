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

use crate::agent::CheckpointSummary;

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindStage {
    Closed,
    Picker,
    Confirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RewindFocus {
    Secondary,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RewindHit {
    Launcher,
    Item(usize),
    Secondary,
    Primary,
}

#[derive(Debug, Clone)]
pub struct RewindUiState {
    stage: RewindStage,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<RewindFocus>,
    clicks: ClickRegionRegistry<RewindHit>,
    reviewed_checkpoint_id: Option<u64>,
}

impl RewindUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(RewindFocus::Secondary);
        focus.register(RewindFocus::Primary);
        focus.set(RewindFocus::Primary);
        Self {
            stage: RewindStage::Closed,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            focus,
            clicks: ClickRegionRegistry::new(),
            reviewed_checkpoint_id: None,
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, RewindStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> RewindStage {
        self.stage
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn open(&mut self, total: usize) {
        self.picker.set_total(total);
        self.picker.select_first();
        self.picker.scroll = 0;
        self.stage = RewindStage::Picker;
        self.reviewed_checkpoint_id = None;
        self.focus.set(RewindFocus::Primary);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = RewindStage::Closed;
        self.reviewed_checkpoint_id = None;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn set_total(&mut self, total: usize) {
        self.picker.set_total(total);
        self.clicks.clear();
        if total == 0 {
            self.close();
        }
    }

    pub fn next_item(&mut self) {
        self.picker.select_next();
    }

    pub fn previous_item(&mut self) {
        self.picker.select_prev();
    }

    pub fn first_item(&mut self) {
        self.picker.select_first();
    }

    pub fn last_item(&mut self) {
        self.picker.select_last();
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: RewindFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<RewindFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
    }

    pub fn review(&mut self, checkpoints: &[CheckpointSummary]) {
        if let Some(checkpoint) = checkpoints.get(self.picker.selected_index) {
            self.reviewed_checkpoint_id = Some(checkpoint.id);
            self.stage = RewindStage::Confirm;
            self.focus.set(RewindFocus::Primary);
        }
    }

    pub fn back(&mut self) {
        self.stage = RewindStage::Picker;
        self.reviewed_checkpoint_id = None;
        self.focus.set(RewindFocus::Primary);
    }

    #[must_use]
    pub const fn reviewed_checkpoint_id(&self) -> Option<u64> {
        self.reviewed_checkpoint_id
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<RewindHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw_launcher(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        enabled: bool,
        count: usize,
    ) {
        let mut state = if enabled {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        state.set_focused(false);
        let label = format!("{} ({count})", text(Text::Rewind));
        let button = Button::new(&label, &state)
            .icon("↶")
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::default());
        let region = button.render_stateful(area, frame.buffer_mut());
        if enabled {
            self.clicks.register(region.area, RewindHit::Launcher);
        }
    }

    pub fn draw_dialog(
        &mut self,
        frame: &mut Frame<'_>,
        checkpoints: &[CheckpointSummary],
        idle: bool,
    ) {
        if !self.is_open() {
            return;
        }
        self.picker.set_total(checkpoints.len());
        let stage = self.stage;
        let reviewed_checkpoint_id = self.reviewed_checkpoint_id;
        let primary_focused = self.focus.is_focused(&RewindFocus::Primary);
        let secondary_focused = self.focus.is_focused(&RewindFocus::Secondary);
        let config = DialogConfig::new(match stage {
            RewindStage::Picker => text(Text::CheckpointHistory),
            RewindStage::Confirm => text(Text::ConfirmRewind),
            RewindStage::Closed => text(Text::Rewind),
        })
        .width_percent(78)
        .height_percent(72)
        .min_size(56, 16)
        .max_size(150, 52)
        .border_color(Color::Cyan)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let dialog = &mut self.dialog;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, dialog, |frame, area, _| match stage {
            RewindStage::Picker => draw_picker(
                frame,
                area,
                checkpoints,
                picker,
                primary_focused,
                secondary_focused,
                clicks,
            ),
            RewindStage::Confirm => draw_confirmation(
                frame,
                area,
                reviewed_checkpoint_id
                    .and_then(|id| checkpoints.iter().find(|checkpoint| checkpoint.id == id)),
                primary_focused,
                secondary_focused,
                idle,
                clicks,
            ),
            RewindStage::Closed => {}
        });
        popup.render(frame);
    }
}

impl Default for RewindUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    checkpoints: &[CheckpointSummary],
    picker: &mut ListPickerState,
    primary_focused: bool,
    secondary_focused: bool,
    clicks: &mut ClickRegionRegistry<RewindHit>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                text(Text::ChooseBoundaryHelp),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(text(Text::RewindNavigationHelp)),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::NewestCheckpointTitle)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    let items = checkpoints
        .iter()
        .map(|checkpoint| {
            let time = checkpoint.created_at.format("%Y-%m-%d %H:%M:%S");
            format!(
                "#{:<4} {} · {:>3} {} · {}",
                checkpoint.id,
                time,
                checkpoint.changed_paths.len(),
                text(Text::FilesLabel),
                sanitize_for_display(&checkpoint.prompt_preview)
            )
        })
        .collect::<Vec<_>>();
    let list = ListPicker::new(&items, picker).style(ListPickerStyle::bracket().bordered(false));
    frame.render_widget(list, inner);
    for visible_row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(visible_row);
        if index >= checkpoints.len() {
            break;
        }
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(visible_row as u16),
                inner.width,
                1,
            ),
            RewindHit::Item(index),
        );
    }
    draw_buttons(
        frame,
        chunks[2],
        text(Text::CancelEsc),
        text(Text::ReviewCheckpoint),
        secondary_focused,
        primary_focused,
        !checkpoints.is_empty(),
        clicks,
    );
}

fn draw_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    checkpoint: Option<&CheckpointSummary>,
    primary_focused: bool,
    secondary_focused: bool,
    idle: bool,
    clicks: &mut ClickRegionRegistry<RewindHit>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(5),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    let Some(checkpoint) = checkpoint else {
        frame.render_widget(
            Paragraph::new(text(Text::CheckpointUnavailable)),
            Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(chunks[2].height),
            ),
        );
        draw_buttons(
            frame,
            chunks[2],
            text(Text::Back),
            text(Text::RewindDialogueFiles),
            secondary_focused,
            primary_focused,
            false,
            clicks,
        );
        return;
    };
    let header = vec![
        Line::from(Span::styled(
            format!("{} #{}?", text(Text::RewindBeforeCheckpoint), checkpoint.id),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(sanitize_for_display(&checkpoint.prompt_preview)),
        Line::from(
            text(Text::DialogueReturns)
                .replace("{count}", &checkpoint.history_entries_before.to_string()),
        ),
        Line::from(text(Text::ManualEditsPreserved)),
        Line::from(text(Text::GitUnaffected)),
    ];
    frame.render_widget(Paragraph::new(header).wrap(Wrap { trim: false }), chunks[0]);
    let paths = if checkpoint.changed_paths.is_empty() {
        vec![Line::from(text(Text::DialogueOnlyRewind))]
    } else {
        checkpoint
            .changed_paths
            .iter()
            .take(200)
            .map(|path| Line::from(format!("• {}", sanitize_for_display(path))))
            .collect()
    };
    frame.render_widget(
        Paragraph::new(paths)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} {} ",
                checkpoint.changed_paths.len(),
                text(Text::AffectedFilesLabel)
            )))
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    draw_buttons(
        frame,
        chunks[2],
        text(Text::Back),
        text(Text::RewindDialogueFiles),
        secondary_focused,
        primary_focused,
        idle,
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    secondary_label: &str,
    primary_label: &str,
    secondary_focused: bool,
    primary_focused: bool,
    primary_enabled: bool,
    clicks: &mut ClickRegionRegistry<RewindHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(22),
        Constraint::Length(2),
        Constraint::Length(30),
        Constraint::Fill(1),
    ])
    .split(area);
    let mut secondary_state = ButtonState::enabled();
    secondary_state.set_focused(secondary_focused);
    let secondary = Button::new(secondary_label, &secondary_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let secondary_region = secondary.render_stateful(columns[1], frame.buffer_mut());
    clicks.register(secondary_region.area, RewindHit::Secondary);

    let mut primary_state = if primary_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    primary_state.set_focused(primary_focused);
    let primary = Button::new(primary_label, &primary_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::danger());
    let primary_region = primary.render_stateful(columns[3], frame.buffer_mut());
    if primary_enabled {
        clicks.register(primary_region.area, RewindHit::Primary);
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use super::{RewindFocus, RewindHit, RewindStage, RewindUiState};
    use crate::agent::CheckpointSummary;

    fn checkpoint() -> CheckpointSummary {
        CheckpointSummary {
            id: 7,
            created_at: Utc::now(),
            prompt_preview: "repair parser".to_owned(),
            changed_paths: vec!["src/parser.rs".to_owned()],
            history_entries_before: 12,
            session_id: Some("session-1".to_owned()),
        }
    }

    #[test]
    fn opening_and_reviewing_never_applies_by_itself() {
        let mut state = RewindUiState::new();
        state.open(2);
        assert_eq!(state.stage(), RewindStage::Picker);
        state.review(&[checkpoint(), checkpoint()]);
        assert_eq!(state.stage(), RewindStage::Confirm);
        state.close();
        assert_eq!(state.stage(), RewindStage::Closed);
    }

    #[test]
    fn empty_picker_closes_when_snapshot_is_pruned() {
        let mut state = RewindUiState::new();
        state.open(1);
        state.set_total(0);
        assert!(!state.is_open());
    }

    #[test]
    fn picker_and_confirmation_have_mouse_regions() -> Result<(), Box<dyn std::error::Error>> {
        let checkpoints = [checkpoint()];
        let mut state = RewindUiState::new();
        state.open(checkpoints.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw_dialog(frame, &checkpoints, true))?;
        for expected in [RewindHit::Item(0), RewindHit::Secondary, RewindHit::Primary] {
            assert!((0..36).any(|row| {
                (0..120).any(|column| state.clicked(column, row) == Some(expected))
            }));
        }

        state.review(&checkpoints);
        state.begin_frame();
        terminal.draw(|frame| state.draw_dialog(frame, &checkpoints, true))?;
        for expected in [RewindHit::Secondary, RewindHit::Primary] {
            assert!((0..36).any(|row| {
                (0..120).any(|column| state.clicked(column, row) == Some(expected))
            }));
        }
        Ok(())
    }

    #[test]
    fn confirmation_stays_bound_to_the_reviewed_checkpoint()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = checkpoint();
        let mut second = checkpoint();
        second.id = 8;
        second.prompt_preview = "newer work".to_owned();
        let checkpoints = [first.clone(), second.clone()];
        let mut state = RewindUiState::new();
        state.open(checkpoints.len());
        state.review(&checkpoints);

        let reordered = [second, first];
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw_dialog(frame, &reordered, true))?;
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("#7?"));
        assert!(!rendered.contains("#8?"));
        Ok(())
    }

    #[test]
    fn tab_and_shift_tab_cycle_both_rewind_actions() {
        let mut state = RewindUiState::new();
        assert_eq!(state.focused(), Some(RewindFocus::Primary));
        state.next_focus();
        assert_eq!(state.focused(), Some(RewindFocus::Secondary));
        state.previous_focus();
        assert_eq!(state.focused(), Some(RewindFocus::Primary));
    }

    #[test]
    fn empty_launcher_keeps_a_compact_visible_count() -> Result<(), Box<dyn std::error::Error>> {
        let mut state = RewindUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(19, 3))?;
        terminal.draw(|frame| state.draw_launcher(frame, frame.area(), false, 0))?;
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(rendered.contains("(0)"), "{rendered:?}");
        Ok(())
    }

    #[test]
    fn snapshot_change_clears_stale_click_regions() -> Result<(), Box<dyn std::error::Error>> {
        let checkpoints = [checkpoint()];
        let mut state = RewindUiState::new();
        state.open(checkpoints.len());
        let mut terminal = Terminal::new(TestBackend::new(120, 36))?;
        terminal.draw(|frame| state.draw_dialog(frame, &checkpoints, true))?;
        let point = (0..36)
            .find_map(|row| {
                (0..120)
                    .find(|column| state.clicked(*column, row) == Some(RewindHit::Item(0)))
                    .map(|column| (column, row))
            })
            .ok_or("checkpoint click region")?;

        state.set_total(checkpoints.len());

        assert_eq!(state.clicked(point.0, point.1), None);
        Ok(())
    }
}
