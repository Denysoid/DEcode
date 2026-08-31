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

use crate::github::GitHubSnapshot;

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

const ANIMATION_STEP: Duration = Duration::from_millis(160);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubStage {
    Closed,
    Browse,
    ConfirmCheckout,
    ConfirmCreate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitHubFocus {
    PullRequests,
    Refresh,
    Open,
    Checkout,
    CreateDraft,
    Cancel,
    Confirm,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GitHubHit {
    PullRequest(usize),
    Refresh,
    Open,
    Checkout,
    CreateDraft,
    Cancel,
    Confirm,
    Close,
}

#[derive(Debug, Clone)]
pub struct GitHubUiState {
    stage: GitHubStage,
    dialog: DialogState<()>,
    picker: ListPickerState,
    pull_request_numbers: Vec<u64>,
    selected_number: Option<u64>,
    confirming_number: Option<u64>,
    focus: FocusManager<GitHubFocus>,
    clicks: ClickRegionRegistry<GitHubHit>,
    animation_frame: usize,
    last_animation_at: Instant,
}

impl GitHubUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        for item in [
            GitHubFocus::PullRequests,
            GitHubFocus::Refresh,
            GitHubFocus::Open,
            GitHubFocus::Checkout,
            GitHubFocus::CreateDraft,
            GitHubFocus::Close,
            GitHubFocus::Cancel,
            GitHubFocus::Confirm,
        ] {
            focus.register(item);
        }
        focus.set(GitHubFocus::PullRequests);
        Self {
            stage: GitHubStage::Closed,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            pull_request_numbers: Vec::new(),
            selected_number: None,
            confirming_number: None,
            focus,
            clicks: ClickRegionRegistry::new(),
            animation_frame: 0,
            last_animation_at: Instant::now(),
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.stage, GitHubStage::Closed)
    }

    #[must_use]
    pub const fn stage(&self) -> GitHubStage {
        self.stage
    }

    pub fn open(&mut self, snapshot: &GitHubSnapshot) {
        self.sync(snapshot);
        self.stage = GitHubStage::Browse;
        self.focus.set(GitHubFocus::PullRequests);
        self.dialog.show();
    }

    pub fn close(&mut self) {
        self.stage = GitHubStage::Closed;
        self.dialog.hide();
        self.clicks.clear();
        self.confirming_number = None;
    }

    pub fn sync(&mut self, snapshot: &GitHubSnapshot) {
        if let Some(number) = self.selected_number
            && let Some(index) = snapshot
                .pull_requests
                .iter()
                .position(|request| request.number == number)
        {
            self.picker.select(index);
        }
        self.picker.set_total(snapshot.pull_requests.len());
        if !snapshot.pull_requests.is_empty()
            && self.picker.selected_index >= snapshot.pull_requests.len()
        {
            self.picker
                .select(snapshot.pull_requests.len().saturating_sub(1));
        }
        self.pull_request_numbers = snapshot
            .pull_requests
            .iter()
            .map(|request| request.number)
            .collect();
        self.update_selected_number();
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
        self.cycle_focus(false);
    }

    pub fn previous_focus(&mut self) {
        self.cycle_focus(true);
    }

    pub fn focus(&mut self, focus: GitHubFocus) {
        self.focus.set(focus);
    }

    #[must_use]
    pub fn focused(&self) -> Option<GitHubFocus> {
        self.focus.current().copied()
    }

    pub fn next_item(&mut self) {
        self.picker.select_next();
        self.update_selected_number();
    }

    pub fn previous_item(&mut self) {
        self.picker.select_prev();
        self.update_selected_number();
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
        self.update_selected_number();
        self.focus.set(GitHubFocus::PullRequests);
    }

    #[must_use]
    pub const fn selected(&self) -> usize {
        self.picker.selected_index
    }

    pub fn confirm_checkout(&mut self, number: u64) {
        self.confirming_number = Some(number);
        self.stage = GitHubStage::ConfirmCheckout;
        self.focus.set(GitHubFocus::Cancel);
    }

    pub fn confirm_create(&mut self) {
        self.confirming_number = None;
        self.stage = GitHubStage::ConfirmCreate;
        self.focus.set(GitHubFocus::Cancel);
    }

    pub fn back(&mut self) {
        self.confirming_number = None;
        self.stage = GitHubStage::Browse;
        self.focus.set(GitHubFocus::PullRequests);
    }

    #[must_use]
    pub const fn confirming_pull_request(&self) -> Option<u64> {
        self.confirming_number
    }

    fn cycle_focus(&mut self, backwards: bool) {
        let values: &[GitHubFocus] = match self.stage {
            GitHubStage::Browse => &[
                GitHubFocus::PullRequests,
                GitHubFocus::Refresh,
                GitHubFocus::Open,
                GitHubFocus::Checkout,
                GitHubFocus::CreateDraft,
                GitHubFocus::Close,
            ],
            GitHubStage::ConfirmCheckout | GitHubStage::ConfirmCreate => {
                &[GitHubFocus::Cancel, GitHubFocus::Confirm]
            }
            GitHubStage::Closed => return,
        };
        let current = self.focus.current().copied();
        let index = values
            .iter()
            .position(|value| Some(*value) == current)
            .unwrap_or(0);
        let next = if backwards {
            index
                .checked_sub(1)
                .unwrap_or(values.len().saturating_sub(1))
        } else {
            (index + 1) % values.len()
        };
        self.focus.set(values[next]);
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<GitHubHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, snapshot: &GitHubSnapshot) {
        if !self.is_open() {
            return;
        }
        self.sync(snapshot);
        let stage = self.stage;
        let selected = self.picker.selected_index;
        let confirming_number = self.confirming_number;
        let focused = self.focus.current().copied();
        let animation_frame = self.animation_frame;
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let config = DialogConfig::new(text(Text::GithubPullRequests))
            .width_percent(82)
            .height_percent(80)
            .min_size(76, 26)
            .max_size(142, 54)
            .border_color(Color::Blue)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| match stage {
            GitHubStage::Browse => draw_browser(
                frame,
                area,
                snapshot,
                selected,
                focused,
                animation_frame,
                picker,
                clicks,
            ),
            GitHubStage::ConfirmCheckout | GitHubStage::ConfirmCreate => draw_confirmation(
                frame,
                area,
                snapshot,
                confirming_number,
                stage,
                focused,
                clicks,
            ),
            GitHubStage::Closed => {}
        });
        popup.render(frame);
    }

    fn update_selected_number(&mut self) {
        self.selected_number = self
            .pull_request_numbers
            .get(self.picker.selected_index)
            .copied();
    }
}

impl Default for GitHubUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_browser(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &GitHubSnapshot,
    selected: usize,
    focused: Option<GitHubFocus>,
    animation_frame: usize,
    picker: &mut ListPickerState,
    clicks: &mut ClickRegionRegistry<GitHubHit>,
) {
    let rows = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(7),
        Constraint::Length(7),
        Constraint::Length(6),
    ])
    .split(area);
    let pulse = ["( )", "(o)", "(*)", "(o)"][animation_frame % 4];
    let repository = snapshot
        .repository
        .as_deref()
        .unwrap_or(text(Text::RepositoryNotLoaded));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("{pulse} {}", sanitize_for_display(repository)),
                Style::default()
                    .fg(Color::LightBlue)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(sanitize_for_display(&snapshot.status)),
        ]),
        rows[0],
    );

    let values = snapshot
        .pull_requests
        .iter()
        .map(|pr| {
            let draft = if pr.draft {
                format!("[{}] ", text(Text::DraftLabel))
            } else {
                String::new()
            };
            sanitize_for_display(&format!(
                "#{} {}{} | {} -> {}",
                pr.number, draft, pr.title, pr.head, pr.base
            ))
        })
        .collect::<Vec<_>>();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::OpenPullRequests)));
    let inner = block.inner(rows[1]);
    frame.render_widget(block, rows[1]);
    picker.ensure_visible(usize::from(inner.height));
    frame.render_widget(
        ListPicker::new(&values, picker).style(ListPickerStyle::bracket().bordered(false)),
        inner,
    );
    for visible in 0..usize::from(inner.height) {
        let index = usize::from(picker.scroll).saturating_add(visible);
        if index >= values.len() {
            break;
        }
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(visible as u16),
                inner.width,
                1,
            ),
            GitHubHit::PullRequest(index),
        );
    }
    let detail = snapshot.pull_requests.get(selected).map_or_else(
        || text(Text::GitHubLoadHelp).to_owned(),
        |pr| {
            format!(
                "#{} {}\n{}: {} | {}: {}{}\n{}",
                pr.number,
                sanitize_for_display(&pr.title),
                text(Text::AuthorLabel),
                sanitize_for_display(&pr.author),
                text(Text::PullRequestStateLabel),
                sanitize_for_display(&pr.state),
                if pr.draft {
                    format!(" | {}", text(Text::DraftLabel))
                } else {
                    String::new()
                },
                sanitize_for_display(&pr.url)
            )
        },
    );
    frame.render_widget(
        Paragraph::new(detail)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::DetailsLabel))),
            )
            .wrap(Wrap { trim: false }),
        rows[2],
    );
    draw_browser_buttons(frame, rows[3], snapshot, focused, clicks);
}

fn draw_browser_buttons(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &GitHubSnapshot,
    focused: Option<GitHubFocus>,
    clicks: &mut ClickRegionRegistry<GitHubHit>,
) {
    let rows = Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).split(area);
    let first = Layout::horizontal([
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
        Constraint::Percentage(25),
    ])
    .split(rows[0]);
    let has_selection = snapshot.enabled && !snapshot.pull_requests.is_empty() && !snapshot.busy;
    draw_button(
        frame,
        first[0],
        text(Text::RefreshLabel),
        !snapshot.busy && snapshot.enabled,
        focused == Some(GitHubFocus::Refresh),
        GitHubHit::Refresh,
        clicks,
    );
    draw_button(
        frame,
        first[1],
        text(Text::OpenLabel),
        has_selection,
        focused == Some(GitHubFocus::Open),
        GitHubHit::Open,
        clicks,
    );
    draw_button(
        frame,
        first[2],
        text(Text::CheckoutLabel),
        has_selection,
        focused == Some(GitHubFocus::Checkout),
        GitHubHit::Checkout,
        clicks,
    );
    draw_button(
        frame,
        first[3],
        text(Text::CreateDraftLabel),
        !snapshot.busy && snapshot.enabled,
        focused == Some(GitHubFocus::CreateDraft),
        GitHubHit::CreateDraft,
        clicks,
    );
    draw_button(
        frame,
        rows[1],
        text(Text::Close),
        true,
        focused == Some(GitHubFocus::Close),
        GitHubHit::Close,
        clicks,
    );
}

fn draw_confirmation(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &GitHubSnapshot,
    confirming_number: Option<u64>,
    stage: GitHubStage,
    focused: Option<GitHubFocus>,
    clicks: &mut ClickRegionRegistry<GitHubHit>,
) {
    let rows = Layout::vertical([Constraint::Min(8), Constraint::Length(3)]).split(area);
    let message = match stage {
        GitHubStage::ConfirmCheckout => confirming_number
            .and_then(|number| {
                snapshot
                    .pull_requests
                    .iter()
                    .find(|request| request.number == number)
            })
            .map_or_else(
                || text(Text::PullRequestDisappeared).to_owned(),
                |pr| {
                    format!(
                        "{} #{}?\n\n{}",
                        text(Text::CheckoutPullRequest),
                        pr.number,
                        text(Text::CheckoutSafetyWarning)
                    )
                },
            ),
        GitHubStage::ConfirmCreate => text(Text::CreateDraftConfirmation).to_owned(),
        GitHubStage::Closed | GitHubStage::Browse => String::new(),
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: false }),
        rows[0],
    );
    let columns = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(24),
        Constraint::Length(2),
        Constraint::Length(24),
        Constraint::Fill(1),
    ])
    .split(rows[1]);
    draw_button(
        frame,
        columns[1],
        text(Text::Cancel),
        true,
        focused == Some(GitHubFocus::Cancel),
        GitHubHit::Cancel,
        clicks,
    );
    draw_button(
        frame,
        columns[3],
        text(Text::ConfirmLabel),
        snapshot.enabled
            && !snapshot.busy
            && (!matches!(stage, GitHubStage::ConfirmCheckout)
                || confirming_number.is_some_and(|number| {
                    snapshot
                        .pull_requests
                        .iter()
                        .any(|request| request.number == number)
                })),
        focused == Some(GitHubFocus::Confirm),
        GitHubHit::Confirm,
        clicks,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    enabled: bool,
    focused: bool,
    hit: GitHubHit,
    clicks: &mut ClickRegionRegistry<GitHubHit>,
) {
    let mut state = if enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    state.set_focused(focused);
    let region = Button::new(label, &state)
        .variant(ButtonVariant::Block)
        .style(if matches!(hit, GitHubHit::Confirm) {
            ButtonStyle::danger()
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

    use crate::github::{GitHubSnapshot, PullRequestSummary};

    use super::{GitHubHit, GitHubUiState};

    #[test]
    fn every_pr_action_has_a_mouse_region_and_tab_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = GitHubSnapshot {
            enabled: true,
            repository: Some("denysoid/decode".to_owned()),
            pull_requests: Arc::from([PullRequestSummary {
                number: 7,
                title: "Safe update".to_owned(),
                state: "OPEN".to_owned(),
                url: "https://github.com/denysoid/DEcode/pull/7".to_owned(),
                head: "feature".to_owned(),
                base: "main".to_owned(),
                author: "denysoid".to_owned(),
                draft: false,
            }]),
            ..GitHubSnapshot::default()
        };
        let mut ui = GitHubUiState::new();
        ui.open(&snapshot);
        let mut terminal = Terminal::new(TestBackend::new(120, 40))?;
        terminal.draw(|frame| ui.draw(frame, &snapshot))?;
        for expected in [
            GitHubHit::Refresh,
            GitHubHit::Open,
            GitHubHit::Checkout,
            GitHubHit::CreateDraft,
            GitHubHit::Close,
        ] {
            assert!(
                (0..40).any(|row| (0..120).any(|column| ui.clicked(column, row) == Some(expected)))
            );
        }
        ui.next_focus();
        assert!(ui.focused().is_some());
        Ok(())
    }

    #[test]
    fn selection_follows_the_pull_request_across_reordering() {
        let mut snapshot = GitHubSnapshot {
            pull_requests: Arc::from([pull_request(7), pull_request(8)]),
            ..GitHubSnapshot::default()
        };
        let mut ui = GitHubUiState::new();
        ui.open(&snapshot);
        ui.select(1);

        snapshot.pull_requests = Arc::from([pull_request(8), pull_request(7)]);
        ui.sync(&snapshot);

        assert_eq!(snapshot.pull_requests[ui.selected()].number, 8);
    }

    fn pull_request(number: u64) -> PullRequestSummary {
        PullRequestSummary {
            number,
            title: format!("PR {number}"),
            state: "OPEN".to_owned(),
            url: format!("https://example.test/{number}"),
            head: format!("head-{number}"),
            base: "main".to_owned(),
            author: "author".to_owned(),
            draft: false,
        }
    }
}
