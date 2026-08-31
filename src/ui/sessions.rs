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

use crate::agent::{SessionId, SessionSummary, side_chat::has_visible_text};

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

const MAX_QUERY_BYTES: usize = 4_096;
const MAX_RENAME_BYTES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStage {
    Closed,
    Picker,
    Actions,
    Rename,
    WorkspaceConfirm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionIntent {
    Resume,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionFocus {
    Close,
    New,
    Actions,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionHit {
    Launcher,
    Item(usize),
    ActionItem(usize),
    ToggleArchived,
    Close,
    New,
    Actions,
    Primary,
}

#[derive(Debug, Clone)]
pub struct SessionUiState {
    stage: SessionStage,
    dialog: DialogState<()>,
    picker: ListPickerState,
    action_picker: ListPickerState,
    focus: FocusManager<SessionFocus>,
    clicks: ClickRegionRegistry<SessionHit>,
    query: String,
    include_archived: bool,
    rename_buffer: String,
    pending_intent: SessionIntent,
    session_ids: Vec<SessionId>,
    target_session_id: Option<SessionId>,
}

impl SessionUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(SessionFocus::Close);
        focus.register(SessionFocus::New);
        focus.register(SessionFocus::Actions);
        focus.register(SessionFocus::Primary);
        focus.set(SessionFocus::Primary);
        Self {
            stage: SessionStage::Closed,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            action_picker: ListPickerState::new(4),
            focus,
            clicks: ClickRegionRegistry::new(),
            query: String::new(),
            include_archived: false,
            rename_buffer: String::new(),
            pending_intent: SessionIntent::Resume,
            session_ids: Vec::new(),
            target_session_id: None,
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, SessionStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> SessionStage {
        self.stage
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    #[must_use]
    pub const fn selected_action(&self) -> usize {
        self.action_picker.selected_index
    }

    #[must_use]
    pub fn focused(&self) -> Option<SessionFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn include_archived(&self) -> bool {
        self.include_archived
    }

    #[must_use]
    pub fn rename_buffer(&self) -> &str {
        &self.rename_buffer
    }

    #[must_use]
    pub const fn pending_intent(&self) -> SessionIntent {
        self.pending_intent
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn open(&mut self, total: usize) {
        self.picker.set_total(total);
        self.picker.select_first();
        self.picker.scroll = 0;
        self.stage = SessionStage::Picker;
        self.session_ids.clear();
        self.target_session_id = None;
        self.focus.set(SessionFocus::Primary);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = SessionStage::Closed;
        self.target_session_id = None;
        self.dialog.hide();
        self.clicks.clear();
    }

    pub fn set_total(&mut self, total: usize) {
        self.picker.set_total(total);
        self.clicks.clear();
    }

    pub fn sync(&mut self, sessions: &[SessionSummary]) {
        let selected_id = self
            .session_ids
            .get(self.picker.selected_index)
            .or(self.target_session_id.as_ref());
        self.picker.set_total(sessions.len());
        if let Some(index) =
            selected_id.and_then(|id| sessions.iter().position(|session| &session.id == id))
        {
            self.picker.select(index);
        } else if sessions.is_empty() {
            self.picker.select_first();
        }
        self.session_ids = sessions.iter().map(|session| session.id.clone()).collect();
        self.clicks.clear();
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.target_session_id = None;
    }

    pub fn next_item(&mut self) {
        match self.stage {
            SessionStage::Actions => self.action_picker.select_next(),
            SessionStage::Picker => self.picker.select_next(),
            SessionStage::Closed | SessionStage::Rename | SessionStage::WorkspaceConfirm => {}
        }
    }

    pub fn previous_item(&mut self) {
        match self.stage {
            SessionStage::Actions => self.action_picker.select_prev(),
            SessionStage::Picker => self.picker.select_prev(),
            SessionStage::Closed | SessionStage::Rename | SessionStage::WorkspaceConfirm => {}
        }
    }

    pub fn first_item(&mut self) {
        match self.stage {
            SessionStage::Actions => self.action_picker.select_first(),
            SessionStage::Picker => self.picker.select_first(),
            SessionStage::Closed | SessionStage::Rename | SessionStage::WorkspaceConfirm => {}
        }
    }

    pub fn last_item(&mut self) {
        match self.stage {
            SessionStage::Actions => self.action_picker.select_last(),
            SessionStage::Picker => self.picker.select_last(),
            SessionStage::Closed | SessionStage::Rename | SessionStage::WorkspaceConfirm => {}
        }
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: SessionFocus) {
        self.focus.set(focus);
    }

    pub fn open_actions(&mut self) {
        if self.picker.total_items > 0 {
            self.target_session_id = self.session_ids.get(self.picker.selected_index).cloned();
            self.action_picker.select_first();
            self.stage = SessionStage::Actions;
            self.focus.set(SessionFocus::Primary);
        }
    }

    pub fn begin_rename(&mut self, current_title: &str) {
        self.rename_buffer.clear();
        self.rename_buffer.push_str(current_title);
        self.stage = SessionStage::Rename;
        self.focus.set(SessionFocus::Primary);
    }

    pub fn begin_workspace_confirmation(&mut self, intent: SessionIntent) {
        self.pending_intent = intent;
        self.stage = SessionStage::WorkspaceConfirm;
        self.focus.set(SessionFocus::Primary);
    }

    pub fn back(&mut self) {
        self.stage = SessionStage::Picker;
        self.target_session_id = None;
        self.focus.set(SessionFocus::Primary);
    }

    pub fn toggle_archived(&mut self) {
        self.include_archived = !self.include_archived;
    }

    pub fn push_query(&mut self, character: char) {
        if !character.is_control()
            && self.query.len().saturating_add(character.len_utf8()) <= MAX_QUERY_BYTES
        {
            self.query.push(character);
            self.picker.select_first();
            self.target_session_id = None;
        }
    }

    pub fn pop_query(&mut self) {
        pop_grapheme(&mut self.query);
        self.picker.select_first();
        self.target_session_id = None;
    }

    pub fn push_query_text(&mut self, value: &str) {
        for character in value.chars() {
            self.push_query(character);
        }
    }

    pub fn push_rename(&mut self, character: char) {
        if !character.is_control()
            && self
                .rename_buffer
                .len()
                .saturating_add(character.len_utf8())
                <= MAX_RENAME_BYTES
        {
            self.rename_buffer.push(character);
        }
    }

    pub fn pop_rename(&mut self) {
        pop_grapheme(&mut self.rename_buffer);
    }

    pub fn push_rename_text(&mut self, value: &str) {
        for character in value.chars() {
            self.push_rename(character);
        }
    }

    pub fn bind_selected(&mut self, sessions: &[SessionSummary]) -> bool {
        self.target_session_id = sessions
            .get(self.picker.selected_index)
            .map(|session| session.id.clone());
        self.target_session_id.is_some()
    }

    #[must_use]
    pub fn selected_session_id(&self) -> Option<&SessionId> {
        self.target_session_id
            .as_ref()
            .or_else(|| self.session_ids.get(self.picker.selected_index))
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<SessionHit> {
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
        let label = format!("{} ({count})", text(Text::SessionsLabel));
        let button = Button::new(&label, &state)
            .icon("◫")
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::default());
        let region = button.render_stateful(area, frame.buffer_mut());
        if enabled {
            self.clicks.register(region.area, SessionHit::Launcher);
        }
    }

    pub fn draw_dialog(
        &mut self,
        frame: &mut Frame<'_>,
        sessions: &[SessionSummary],
        current_id: Option<&crate::agent::SessionId>,
        editable: bool,
    ) {
        if !self.is_open() {
            return;
        }
        self.sync(sessions);
        let stage = self.stage;
        let selected = self.picker.selected_index;
        let target_session_id = self.target_session_id.clone();
        let focused = self.focus.current().copied();
        let query = self.query.clone();
        let include_archived = self.include_archived;
        let rename = self.rename_buffer.clone();
        let pending_intent = self.pending_intent;
        let config = DialogConfig::new(match stage {
            SessionStage::Picker | SessionStage::Closed => text(Text::Sessions),
            SessionStage::Actions => text(Text::SessionActions),
            SessionStage::Rename => text(Text::RenameSession),
            SessionStage::WorkspaceConfirm => text(Text::DifferentWorkspace),
        })
        .width_percent(84)
        .height_percent(78)
        .min_size(64, 18)
        .max_size(170, 58)
        .border_color(Color::Cyan)
        .focused_border_color(Color::LightCyan)
        .close_on_escape(false)
        .close_on_outside_click(false)
        .no_buttons();
        let dialog = &mut self.dialog;
        let picker = &mut self.picker;
        let action_picker = &mut self.action_picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, dialog, |frame, area, _| match stage {
            SessionStage::Picker => draw_picker(
                frame,
                area,
                sessions,
                current_id,
                picker,
                focused,
                clicks,
                &query,
                include_archived,
                editable,
            ),
            SessionStage::Actions => draw_actions(
                frame,
                area,
                match target_session_id.as_ref() {
                    Some(id) => sessions.iter().find(|session| &session.id == id),
                    None => sessions.get(selected),
                },
                action_picker,
                focused,
                editable,
                clicks,
            ),
            SessionStage::Rename => draw_rename(frame, area, &rename, focused, editable, clicks),
            SessionStage::WorkspaceConfirm => draw_workspace_confirmation(
                frame,
                area,
                target_session_id
                    .as_ref()
                    .and_then(|id| sessions.iter().find(|session| &session.id == id)),
                pending_intent,
                focused,
                editable,
                clicks,
            ),
            SessionStage::Closed => {}
        });
        popup.render(frame);
    }
}

fn pop_grapheme(value: &mut String) {
    if let Some((index, _)) =
        UnicodeSegmentation::grapheme_indices(value.as_str(), true).next_back()
    {
        value.truncate(index);
    }
}

impl Default for SessionUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_picker(
    frame: &mut Frame<'_>,
    area: Rect,
    sessions: &[SessionSummary],
    current_id: Option<&crate::agent::SessionId>,
    picker: &mut ListPickerState,
    focused: Option<SessionFocus>,
    clicks: &mut ClickRegionRegistry<SessionHit>,
    query: &str,
    include_archived: bool,
    editable: bool,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(area);
    let archived = if include_archived { "☑" } else { "☐" };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    text(Text::SearchPrefix),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(if query.is_empty() {
                    text(Text::TypeToFilter)
                } else {
                    ""
                }),
                Span::raw(if query.is_empty() {
                    String::new()
                } else {
                    sanitize_for_display(query)
                }),
            ]),
            Line::from(format!(
                "{archived} {}  •  Tab  •  ↑/↓",
                text(Text::ShowArchived)
            )),
            Line::from(text(Text::PinnedSessionsHelp)),
        ])
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    clicks.register(chunks[0], SessionHit::ToggleArchived);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::SessionListTitle)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    let items = sessions
        .iter()
        .map(|session| {
            let current = if current_id == Some(&session.id) {
                "▶"
            } else {
                " "
            };
            let pin = if session.pinned { "★" } else { " " };
            let archived = if session.archived {
                format!(" [{}]", text(Text::ArchivedLabel))
            } else {
                String::new()
            };
            format!(
                "{current}{pin} {}{}  • {}  • {}",
                sanitize_for_display(&session.title),
                archived,
                session.updated_at.format("%Y-%m-%d %H:%M"),
                sanitize_for_display(&session.workspace_root.to_string_lossy())
            )
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        ListPicker::new(&items, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for visible_row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(visible_row);
        if index >= sessions.len() {
            break;
        }
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(visible_row as u16),
                inner.width,
                1,
            ),
            SessionHit::Item(index),
        );
    }
    draw_four_buttons(
        frame,
        chunks[2],
        [
            text(Text::Close),
            text(Text::New),
            text(Text::Actions),
            text(Text::Resume),
        ],
        focused,
        [
            true,
            editable,
            !sessions.is_empty(),
            editable && !sessions.is_empty(),
        ],
        clicks,
    );
}

fn draw_actions(
    frame: &mut Frame<'_>,
    area: Rect,
    session: Option<&SessionSummary>,
    picker: &mut ListPickerState,
    focused: Option<SessionFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<SessionHit>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(5),
        Constraint::Length(3),
    ])
    .split(area);
    let Some(session) = session else {
        frame.render_widget(
            Paragraph::new(text(Text::SessionUnavailable)),
            Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(chunks[2].height),
            ),
        );
        draw_four_buttons(
            frame,
            chunks[2],
            [text(Text::Back), "", "", text(Text::Apply)],
            focused,
            [true, false, false, false],
            clicks,
        );
        return;
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                sanitize_for_display(&session.title),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("{}: {}", text(Text::IdentifierLabel), session.id)),
            Line::from(format!(
                "{} {}",
                session.history_entries,
                text(Text::HistoryEntries)
            )),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    let labels = vec![
        text(Text::ForkNewSession).to_owned(),
        text(Text::Rename).to_owned(),
        if session.pinned {
            text(Text::Unpin)
        } else {
            text(Text::Pin)
        }
        .to_owned(),
        if session.archived {
            text(Text::RestoreArchive)
        } else {
            text(Text::Archive)
        }
        .to_owned(),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ChooseAction)));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    picker.ensure_visible(usize::from(inner.height));
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for index in 0..labels.len().min(usize::from(inner.height)) {
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(index as u16),
                inner.width,
                1,
            ),
            SessionHit::ActionItem(index),
        );
    }
    draw_four_buttons(
        frame,
        chunks[2],
        [text(Text::Back), "", "", text(Text::Apply)],
        focused,
        [true, false, false, editable],
        clicks,
    );
}

fn draw_rename(
    frame: &mut Frame<'_>,
    area: Rect,
    rename: &str,
    focused: Option<SessionFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<SessionHit>,
) {
    let chunks = Layout::vertical([Constraint::Min(5), Constraint::Length(3)]).split(area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(text(Text::DurableTitleHelp)),
            Line::from(""),
            Line::from(Span::styled(
                sanitize_for_display(rename),
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD),
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", text(Text::SessionTitle))),
        )
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    draw_four_buttons(
        frame,
        chunks[1],
        [text(Text::Cancel), "", "", text(Text::SaveName)],
        focused,
        [
            true,
            false,
            false,
            editable && rename.len() <= MAX_RENAME_BYTES && has_visible_text(rename),
        ],
        clicks,
    );
}

fn draw_workspace_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    session: Option<&SessionSummary>,
    intent: SessionIntent,
    focused: Option<SessionFocus>,
    editable: bool,
    clicks: &mut ClickRegionRegistry<SessionHit>,
) {
    let chunks = Layout::vertical([Constraint::Min(7), Constraint::Length(3)]).split(area);
    let Some(session) = session else {
        frame.render_widget(
            Paragraph::new(text(Text::SessionUnavailable)),
            Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(chunks[1].height),
            ),
        );
        draw_four_buttons(
            frame,
            chunks[1],
            [text(Text::Cancel), "", "", text(Text::Continue)],
            focused,
            [true, false, false, false],
            clicks,
        );
        return;
    };
    let confirmation = match intent {
        SessionIntent::Resume => text(Text::ResumeOtherWorkspace),
        SessionIntent::Fork => text(Text::ForkOtherWorkspace),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                confirmation,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!(
                "{}: {}",
                text(Text::SavedRoot),
                sanitize_for_display(&session.workspace_root.to_string_lossy())
            )),
            Line::from(text(Text::OldFileReferencesWarning)),
            Line::from(text(Text::SessionOpeningNoFileChanges)),
        ])
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    draw_four_buttons(
        frame,
        chunks[1],
        [text(Text::Cancel), "", "", text(Text::Continue)],
        focused,
        [true, false, false, editable],
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_four_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    labels: [&str; 4],
    focused: Option<SessionFocus>,
    enabled: [bool; 4],
    clicks: &mut ClickRegionRegistry<SessionHit>,
) {
    let columns = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(area);
    let definitions = [
        (SessionFocus::Close, SessionHit::Close),
        (SessionFocus::New, SessionHit::New),
        (SessionFocus::Actions, SessionHit::Actions),
        (SessionFocus::Primary, SessionHit::Primary),
    ];
    for (index, ((button_focus, hit), label)) in definitions.into_iter().zip(labels).enumerate() {
        if label.is_empty() {
            continue;
        }
        let button_enabled = enabled[index];
        let mut state = if button_enabled {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        state.set_focused(focused == Some(button_focus));
        let style = if matches!(button_focus, SessionFocus::Primary) {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        };
        let button = Button::new(label, &state)
            .variant(ButtonVariant::Block)
            .style(style);
        let region = button.render_stateful(columns[index], frame.buffer_mut());
        if button_enabled {
            clicks.register(region.area, hit);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf};

    use chrono::Utc;
    use ratatui::{Terminal, backend::TestBackend};

    use crate::agent::{SessionId, SessionSummary};

    use super::{SessionFocus, SessionHit, SessionIntent, SessionStage, SessionUiState};

    const WIDTH: u16 = 110;
    const HEIGHT: u16 = 36;

    fn session() -> Result<SessionSummary, Box<dyn std::error::Error>> {
        Ok(SessionSummary {
            id: SessionId::parse("session-1")?,
            title: "Durable session".to_owned(),
            preview: "preview".to_owned(),
            workspace_root: PathBuf::from("D:/workspace"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            pinned: true,
            archived: false,
            history_entries: 7,
            parent_session_id: None,
            recovered_records: 0,
        })
    }

    fn named_session(
        id: &'static str,
        title: &'static str,
    ) -> Result<SessionSummary, Box<dyn std::error::Error>> {
        let mut session = session()?;
        session.id = SessionId::parse(id)?;
        session.title = title.to_owned();
        Ok(session)
    }

    fn rendered_hits(ui: &SessionUiState) -> HashSet<SessionHit> {
        let mut hits = HashSet::new();
        for row in 0..HEIGHT {
            for column in 0..WIDTH {
                if let Some(hit) = ui.clicked(column, row) {
                    hits.insert(hit);
                }
            }
        }
        hits
    }

    #[test]
    fn session_dialog_requires_explicit_primary_action() {
        let mut ui = SessionUiState::new();
        ui.open(2);
        assert_eq!(ui.stage(), SessionStage::Picker);
        ui.open_actions();
        assert_eq!(ui.stage(), SessionStage::Actions);
        ui.begin_workspace_confirmation(SessionIntent::Resume);
        assert_eq!(ui.stage(), SessionStage::WorkspaceConfirm);
        ui.close();
        assert_eq!(ui.stage(), SessionStage::Closed);
    }

    #[test]
    fn archived_and_search_state_are_explicit() {
        let mut ui = SessionUiState::new();
        ui.push_query('b');
        ui.push_query('u');
        ui.pop_query();
        ui.toggle_archived();
        assert_eq!(ui.query(), "b");
        assert!(ui.include_archived());
    }

    #[test]
    fn text_editors_are_byte_bounded_and_grapheme_aware() {
        let mut ui = SessionUiState::new();
        for _ in 0..4_095 {
            ui.push_query('a');
        }
        ui.push_query('é');
        assert_eq!(ui.query().len(), 4_095);

        ui.begin_rename("");
        for _ in 0..511 {
            ui.push_rename('a');
        }
        ui.push_rename('é');
        assert_eq!(ui.rename_buffer().len(), 511);
        ui.begin_rename("👩‍💻");
        ui.pop_rename();
        assert!(ui.rename_buffer().is_empty());
    }

    #[test]
    fn query_controls_are_escaped_before_rendering() -> Result<(), Box<dyn std::error::Error>> {
        let sessions = [session()?];
        let mut ui = SessionUiState::new();
        ui.open(sessions.len());
        ui.push_query('\u{202e}');
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))?;

        terminal.draw(|frame| ui.draw_dialog(frame, &sessions, Some(&sessions[0].id), true))?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains('\u{202e}'));
        assert!(rendered.contains("<U+202E>"));
        Ok(())
    }

    #[test]
    fn actions_stay_bound_to_the_selected_session_after_reordering()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = named_session("session-1", "First session")?;
        let second = named_session("session-2", "Second session")?;
        let sessions = [first.clone(), second.clone()];
        let mut ui = SessionUiState::new();
        ui.open(sessions.len());
        ui.sync(&sessions);
        ui.open_actions();
        let reordered = [second, first];
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))?;

        terminal.draw(|frame| ui.draw_dialog(frame, &reordered, None, true))?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("First session"));
        assert!(!rendered.contains("Second session"));
        Ok(())
    }

    #[test]
    fn removed_action_target_is_not_replaced_by_another_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let first = named_session("session-1", "First session")?;
        let second = named_session("session-2", "Second session")?;
        let sessions = [first, second.clone()];
        let mut ui = SessionUiState::new();
        ui.open(sessions.len());
        ui.sync(&sessions);
        ui.open_actions();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))?;

        terminal.draw(|frame| ui.draw_dialog(frame, &[second], None, true))?;

        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(!rendered.contains("Second session"));
        assert!(!rendered_hits(&ui).contains(&SessionHit::Primary));
        Ok(())
    }

    #[test]
    fn every_session_stage_exposes_mouse_actions() -> Result<(), Box<dyn std::error::Error>> {
        let sessions = [session()?];
        let mut ui = SessionUiState::new();
        let mut terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))?;

        ui.open(sessions.len());
        terminal.draw(|frame| ui.draw_dialog(frame, &sessions, Some(&sessions[0].id), true))?;
        let hits = rendered_hits(&ui);
        for expected in [
            SessionHit::ToggleArchived,
            SessionHit::Item(0),
            SessionHit::Close,
            SessionHit::New,
            SessionHit::Actions,
            SessionHit::Primary,
        ] {
            assert!(hits.contains(&expected), "missing picker hit {expected:?}");
        }

        ui.begin_frame();
        ui.open_actions();
        terminal.draw(|frame| ui.draw_dialog(frame, &sessions, Some(&sessions[0].id), true))?;
        let hits = rendered_hits(&ui);
        for expected in [
            SessionHit::ActionItem(0),
            SessionHit::ActionItem(1),
            SessionHit::ActionItem(2),
            SessionHit::ActionItem(3),
            SessionHit::Close,
            SessionHit::Primary,
        ] {
            assert!(hits.contains(&expected), "missing actions hit {expected:?}");
        }

        ui.begin_frame();
        ui.begin_rename(&sessions[0].title);
        terminal.draw(|frame| ui.draw_dialog(frame, &sessions, Some(&sessions[0].id), true))?;
        let hits = rendered_hits(&ui);
        assert!(hits.contains(&SessionHit::Close));
        assert!(hits.contains(&SessionHit::Primary));

        ui.begin_frame();
        ui.begin_workspace_confirmation(SessionIntent::Fork);
        terminal.draw(|frame| ui.draw_dialog(frame, &sessions, Some(&sessions[0].id), true))?;
        let hits = rendered_hits(&ui);
        assert!(hits.contains(&SessionHit::Close));
        assert!(hits.contains(&SessionHit::Primary));
        Ok(())
    }

    #[test]
    fn session_actions_have_tab_and_shift_tab_fallbacks() {
        let mut ui = SessionUiState::new();
        ui.open(1);
        assert_eq!(ui.focused(), Some(SessionFocus::Primary));
        ui.next_focus();
        assert_eq!(ui.focused(), Some(SessionFocus::Close));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(SessionFocus::Primary));
    }
}
