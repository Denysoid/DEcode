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

use crate::agent::{ShellCommandGrant, ShellPermissionSnapshot};

use super::{
    i18n::{Text, text},
    render::{sanitize_for_display, truncate_for_display},
    syntax,
};

const ANIMATION_STEP: Duration = Duration::from_millis(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionFocus {
    Grants,
    Revoke,
    Clear,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionHit {
    Grant(usize),
    Revoke,
    Clear,
    Close,
}

#[derive(Debug, Clone)]
pub struct PermissionUiState {
    open: bool,
    dialog: DialogState<()>,
    picker: ListPickerState,
    grant_ids: Vec<u64>,
    focus: FocusManager<PermissionFocus>,
    clicks: ClickRegionRegistry<PermissionHit>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl PermissionUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(PermissionFocus::Grants);
        focus.register(PermissionFocus::Revoke);
        focus.register(PermissionFocus::Clear);
        focus.register(PermissionFocus::Close);
        focus.set(PermissionFocus::Grants);
        Self {
            open: false,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            grant_ids: Vec::new(),
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

    pub fn open(&mut self, total: usize) {
        self.open = true;
        self.picker.set_total(total);
        self.grant_ids.clear();
        if total > 0 {
            self.picker.select_first();
        }
        self.focus.set(PermissionFocus::Grants);
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

    pub fn sync(&mut self, snapshot: &ShellPermissionSnapshot) {
        let selected_id = self.grant_ids.get(self.picker.selected_index).copied();
        let grant_ids = snapshot
            .grants
            .iter()
            .map(|grant| grant.id)
            .collect::<Vec<_>>();
        self.picker.set_total(snapshot.grants.len());
        if let Some(index) =
            selected_id.and_then(|selected_id| grant_ids.iter().position(|id| *id == selected_id))
        {
            self.picker.select(index);
        } else if !snapshot.grants.is_empty() && self.picker.selected_index >= snapshot.grants.len()
        {
            self.picker.select(snapshot.grants.len().saturating_sub(1));
        }
        self.grant_ids = grant_ids;
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
        self.focus.set(PermissionFocus::Grants);
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    pub fn next_focus(&mut self) {
        self.focus.next();
    }

    pub fn previous_focus(&mut self) {
        self.focus.prev();
    }

    pub fn focus(&mut self, focus: PermissionFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<PermissionFocus> {
        self.focus.current().copied()
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PermissionHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        snapshot: &ShellPermissionSnapshot,
        editable: bool,
    ) {
        if !self.open {
            return;
        }
        self.sync(snapshot);
        let selected = self.picker.selected_index;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::SessionPermissions))
            .width_percent(84)
            .height_percent(72)
            .min_size(70, 19)
            .max_size(160, 48)
            .border_color(Color::Yellow)
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

impl Default for PermissionUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_content(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &ShellPermissionSnapshot,
    selected: usize,
    picker: &mut ListPickerState,
    focused: Option<PermissionFocus>,
    animation_frame: usize,
    editable: bool,
    clicks: &mut ClickRegionRegistry<PermissionHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(8),
        Constraint::Length(3),
    ])
    .split(area);
    let pulse = ["[= ]", "[==]", "[ =]", "[==]"][animation_frame % 4];
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    format!("{pulse} {}", text(Text::SessionOnly)),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "  |  {} {}  |  {} {}",
                    snapshot.grants.len(),
                    text(Text::ExactCommands),
                    text(Text::Revision),
                    snapshot.revision
                )),
            ]),
            Line::from(text(Text::SessionGrantHelp)),
            Line::from(text(Text::ForcedConfirmationHelp)),
        ])
        .wrap(Wrap { trim: false }),
        rows[0],
    );

    let columns =
        Layout::horizontal([Constraint::Percentage(42), Constraint::Percentage(58)]).split(rows[1]);
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::ExactGrants)));
    let list_inner = list_block.inner(columns[0]);
    frame.render_widget(list_block, columns[0]);
    let labels = snapshot.grants.iter().map(grant_label).collect::<Vec<_>>();
    let viewport = usize::from(list_inner.height);
    picker.ensure_visible(viewport);
    frame.render_widget(
        ListPicker::new(&labels, picker).style(ListPickerStyle::bracket().bordered(false)),
        list_inner,
    );
    for row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(row);
        if index >= snapshot.grants.len() {
            break;
        }
        clicks.register(
            Rect::new(
                list_inner.x,
                list_inner.y.saturating_add(row as u16),
                list_inner.width,
                1,
            ),
            PermissionHit::Grant(index),
        );
    }

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::SanitizedCommandPreview)));
    let detail_inner = detail_block.inner(columns[1]);
    frame.render_widget(detail_block, columns[1]);
    draw_grant_detail(frame, detail_inner, snapshot.grants.get(selected));

    let buttons = Layout::horizontal([
        Constraint::Length(20),
        Constraint::Length(2),
        Constraint::Length(20),
        Constraint::Fill(1),
        Constraint::Length(16),
    ])
    .split(rows[2]);
    let has_grants = !snapshot.grants.is_empty();
    render_button(
        frame,
        buttons[0],
        text(Text::RevokeSelected),
        PermissionHit::Revoke,
        focused == Some(PermissionFocus::Revoke),
        ButtonStyle::danger(),
        has_grants && editable,
        clicks,
    );
    render_button(
        frame,
        buttons[2],
        text(Text::RevokeAll),
        PermissionHit::Clear,
        focused == Some(PermissionFocus::Clear),
        ButtonStyle::danger(),
        has_grants && editable,
        clicks,
    );
    render_button(
        frame,
        buttons[4],
        text(Text::CloseEsc),
        PermissionHit::Close,
        focused == Some(PermissionFocus::Close),
        ButtonStyle::default(),
        true,
        clicks,
    );
}

fn grant_label(grant: &ShellCommandGrant) -> String {
    let safe = sanitize_for_display(&grant.command.replace(['\r', '\n'], " "));
    let preview = truncate_for_display(&safe, 54);
    format!("#{}  {preview}", grant.id)
}

fn draw_grant_detail(frame: &mut Frame<'_>, area: Rect, grant: Option<&ShellCommandGrant>) {
    let Some(grant) = grant else {
        frame.render_widget(Paragraph::new(text(Text::NoSessionGrantsHelp)), area);
        return;
    };
    let digest = grant
        .command_digest
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let safe_command = sanitize_for_display(&grant.command);
    // Security invariant: untrusted command text is sanitized before it is
    // passed to the syntax highlighter or terminal renderer.
    let highlighted = syntax::highlight_source("command.sh", &safe_command);
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{} #{}", text(Text::GrantLabel), grant.id),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            "{} {} / {} {}",
            text(Text::GrantedAtTurn),
            grant.granted_turn_id,
            text(Text::ActionLabel),
            grant.granted_action_id
        )),
        Line::from(format!("{}: {digest}", text(Text::Sha256Label))),
        Line::from(""),
    ];
    if let Some(highlighted) = highlighted {
        lines.extend(highlighted.into_iter().map(Line::from));
    } else {
        lines.extend(safe_command.lines().map(|line| Line::from(line.to_owned())));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[allow(clippy::too_many_arguments)]
fn render_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    hit: PermissionHit,
    focused: bool,
    style: ButtonStyle,
    enabled: bool,
    clicks: &mut ClickRegionRegistry<PermissionHit>,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ratatui::{Terminal, backend::TestBackend};

    use crate::{
        agent::{ShellCommandGrant, ShellPermissionSnapshot},
        tools::CommandDigest,
    };

    use super::{PermissionFocus, PermissionHit, PermissionUiState};

    fn snapshot() -> ShellPermissionSnapshot {
        ShellPermissionSnapshot {
            revision: 1,
            grants: Arc::from([ShellCommandGrant {
                id: 7,
                command: "cargo test --all-targets".to_owned(),
                command_digest: CommandDigest::for_command("cargo test --all-targets"),
                granted_turn_id: 2,
                granted_action_id: 3,
            }]),
        }
    }

    #[test]
    fn permission_manager_has_mouse_regions_and_tab_focus() -> Result<(), Box<dyn std::error::Error>>
    {
        let snapshot = snapshot();
        let mut ui = PermissionUiState::new();
        ui.open(snapshot.grants.len());
        assert_eq!(ui.focused(), Some(PermissionFocus::Grants));
        ui.next_focus();
        assert_eq!(ui.focused(), Some(PermissionFocus::Revoke));
        ui.next_focus();
        assert_eq!(ui.focused(), Some(PermissionFocus::Clear));
        ui.previous_focus();
        assert_eq!(ui.focused(), Some(PermissionFocus::Revoke));

        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;
        let mut grant = false;
        let mut revoke = false;
        let mut clear = false;
        let mut close = false;
        for row in 0..32 {
            for column in 0..110 {
                match ui.clicked(column, row) {
                    Some(PermissionHit::Grant(0)) => grant = true,
                    Some(PermissionHit::Revoke) => revoke = true,
                    Some(PermissionHit::Clear) => clear = true,
                    Some(PermissionHit::Close) => close = true,
                    Some(PermissionHit::Grant(_)) | None => {}
                }
            }
        }
        assert!(grant && revoke && clear && close);
        Ok(())
    }

    #[test]
    fn destructive_buttons_have_no_mouse_regions_without_grants()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = ShellPermissionSnapshot::default();
        let mut ui = PermissionUiState::new();
        ui.open(0);
        let backend = TestBackend::new(110, 32);
        let mut terminal = Terminal::new(backend)?;
        terminal.draw(|frame| ui.draw(frame, &snapshot, true))?;
        for row in 0..32 {
            for column in 0..110 {
                assert!(!matches!(
                    ui.clicked(column, row),
                    Some(PermissionHit::Revoke | PermissionHit::Clear)
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn selection_follows_grant_identity_across_snapshot_updates() {
        let grant = |id, command: &str| ShellCommandGrant {
            id,
            command: command.to_owned(),
            command_digest: CommandDigest::for_command(command),
            granted_turn_id: 1,
            granted_action_id: id,
        };
        let initial = ShellPermissionSnapshot {
            revision: 1,
            grants: Arc::from([grant(1, "first"), grant(2, "second")]),
        };
        let updated = ShellPermissionSnapshot {
            revision: 2,
            grants: Arc::from([grant(3, "new"), grant(1, "first"), grant(2, "second")]),
        };
        let mut ui = PermissionUiState::new();
        ui.open(initial.grants.len());
        ui.sync(&initial);
        ui.select(1);

        ui.sync(&updated);

        assert_eq!(updated.grants[ui.selected_index()].id, 2);
    }

    #[test]
    fn grant_labels_do_not_split_grapheme_clusters() {
        let command = format!("{}e\u{301}", "a".repeat(53));
        let grant = ShellCommandGrant {
            id: 1,
            command: command.clone(),
            command_digest: CommandDigest::for_command(&command),
            granted_turn_id: 1,
            granted_action_id: 1,
        };

        assert!(super::grant_label(&grant).ends_with("e\u{301}"));
    }
}
