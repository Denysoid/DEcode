use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};

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
use unicode_width::UnicodeWidthStr;

use crate::agent::automation::{AutomationSource, CustomCommandSummary};

use super::{
    i18n::{Text, text},
    render::sanitize_for_display,
};

const MAX_QUERY_BYTES: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteMode {
    Closed,
    Commands,
    Files,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteFocus {
    Close,
    Primary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaletteHit {
    Item(usize),
    Close,
    Primary,
}

#[derive(Debug, Clone, Copy)]
pub struct PaletteCommand {
    pub id: &'static str,
}

pub const COMMANDS: [PaletteCommand; 35] = [
    PaletteCommand { id: "new_session" },
    PaletteCommand { id: "sessions" },
    PaletteCommand { id: "rewind" },
    PaletteCommand { id: "runtime" },
    PaletteCommand { id: "language" },
    PaletteCommand { id: "rerun_setup" },
    PaletteCommand { id: "mcp" },
    PaletteCommand { id: "lsp" },
    PaletteCommand { id: "code_index" },
    PaletteCommand { id: "privacy" },
    PaletteCommand { id: "permissions" },
    PaletteCommand {
        id: "auto_approval",
    },
    PaletteCommand { id: "usage" },
    PaletteCommand {
        id: "notifications",
    },
    PaletteCommand { id: "reviews" },
    PaletteCommand { id: "side_chat" },
    PaletteCommand { id: "follow_ups" },
    PaletteCommand { id: "modes" },
    PaletteCommand { id: "instructions" },
    PaletteCommand { id: "skills" },
    PaletteCommand { id: "plugins" },
    PaletteCommand { id: "automation" },
    PaletteCommand { id: "tab_chat" },
    PaletteCommand { id: "tab_activity" },
    PaletteCommand { id: "tab_diff" },
    PaletteCommand { id: "tab_plan" },
    PaletteCommand { id: "tab_agents" },
    PaletteCommand { id: "tab_terminal" },
    PaletteCommand { id: "terminal_new" },
    PaletteCommand {
        id: "terminal_stop",
    },
    PaletteCommand {
        id: "terminal_close",
    },
    PaletteCommand { id: "toggle_left" },
    PaletteCommand { id: "toggle_right" },
    PaletteCommand { id: "jump_latest" },
    PaletteCommand { id: "shortcuts" },
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteCommandSelection {
    BuiltIn(&'static str),
    Custom(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteCommandMatch {
    pub selection: PaletteCommandSelection,
    pub label: String,
    pub hint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowserEntryKind {
    Workspace,
    Home,
    Desktop,
    Downloads,
    Drive,
    Parent,
    Directory,
    File { size: u64 },
}

#[derive(Debug, Clone)]
struct BrowserEntry {
    path: PathBuf,
    name: String,
    kind: BrowserEntryKind,
}

impl BrowserEntry {
    fn is_directory(&self) -> bool {
        !matches!(self.kind, BrowserEntryKind::File { .. })
    }

    fn label(&self, selected: bool) -> String {
        match self.kind {
            BrowserEntryKind::Workspace => {
                format!(
                    "[{}] {}",
                    text(Text::WorkspaceLocation),
                    self.path.display()
                )
            }
            BrowserEntryKind::Home => {
                format!("[{}] {}", text(Text::HomeLocation), self.path.display())
            }
            BrowserEntryKind::Desktop => {
                format!("[{}] {}", text(Text::DesktopLocation), self.path.display())
            }
            BrowserEntryKind::Downloads => {
                format!(
                    "[{}] {}",
                    text(Text::DownloadsLocation),
                    self.path.display()
                )
            }
            BrowserEntryKind::Drive => format!("[{}] {}", text(Text::DriveLocation), self.name),
            BrowserEntryKind::Parent => format!("[..] {}", self.path.display()),
            BrowserEntryKind::Directory => format!("[DIR] {}/", self.name),
            BrowserEntryKind::File { size } => format!(
                "[{}] {} · {}",
                if selected { 'x' } else { ' ' },
                self.name,
                format_file_size(size)
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct FileBrowserState {
    workspace_root: PathBuf,
    current_dir: PathBuf,
    entries: Vec<BrowserEntry>,
    selected_files: BTreeSet<PathBuf>,
}

impl FileBrowserState {
    fn open(workspace_root: &Path) -> io::Result<Self> {
        let workspace_root = absolute_path(workspace_root)?;
        let mut browser = Self {
            current_dir: workspace_root.clone(),
            workspace_root,
            entries: Vec::new(),
            selected_files: BTreeSet::new(),
        };
        browser.navigate_to(browser.current_dir.clone())?;
        Ok(browser)
    }

    fn navigate_to(&mut self, path: PathBuf) -> io::Result<()> {
        let path = absolute_path(&path)?;
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file browser location must be a real directory",
            ));
        }
        let entries = load_browser_entries(&path, &self.workspace_root)?;
        self.current_dir = path;
        self.entries = entries;
        Ok(())
    }

    fn visible_indices(&self, query: &str) -> Vec<usize> {
        let query = query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                query.is_empty()
                    || entry.name.to_lowercase().contains(&query)
                    || entry.path.to_string_lossy().to_lowercase().contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn load_browser_entries(
    current_dir: &Path,
    workspace_root: &Path,
) -> io::Result<Vec<BrowserEntry>> {
    let mut entries = Vec::new();
    let mut shortcuts = BTreeSet::new();
    add_browser_shortcut(
        &mut entries,
        &mut shortcuts,
        current_dir,
        workspace_root,
        BrowserEntryKind::Workspace,
    );
    if let Some(user_dirs) = directories::UserDirs::new() {
        add_browser_shortcut(
            &mut entries,
            &mut shortcuts,
            current_dir,
            user_dirs.home_dir(),
            BrowserEntryKind::Home,
        );
        if let Some(path) = user_dirs.desktop_dir() {
            add_browser_shortcut(
                &mut entries,
                &mut shortcuts,
                current_dir,
                path,
                BrowserEntryKind::Desktop,
            );
        }
        if let Some(path) = user_dirs.download_dir() {
            add_browser_shortcut(
                &mut entries,
                &mut shortcuts,
                current_dir,
                path,
                BrowserEntryKind::Downloads,
            );
        }
    }
    for root in system_roots() {
        add_browser_shortcut(
            &mut entries,
            &mut shortcuts,
            current_dir,
            &root,
            BrowserEntryKind::Drive,
        );
    }
    if let Some(parent) = current_dir.parent() {
        entries.push(BrowserEntry {
            path: parent.to_path_buf(),
            name: "..".to_owned(),
            kind: BrowserEntryKind::Parent,
        });
    }

    let mut directories = Vec::new();
    let mut files = Vec::new();
    for candidate in std::fs::read_dir(current_dir)? {
        let Ok(candidate) = candidate else {
            continue;
        };
        let Ok(file_type) = candidate.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = candidate.path();
        let name = candidate.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            directories.push(BrowserEntry {
                path,
                name,
                kind: BrowserEntryKind::Directory,
            });
        } else if file_type.is_file() {
            let size = candidate.metadata().map_or(0, |metadata| metadata.len());
            files.push(BrowserEntry {
                path,
                name,
                kind: BrowserEntryKind::File { size },
            });
        }
    }
    directories.sort_by_cached_key(|entry| entry.name.to_lowercase());
    files.sort_by_cached_key(|entry| entry.name.to_lowercase());
    entries.extend(directories);
    entries.extend(files);
    Ok(entries)
}

fn add_browser_shortcut(
    entries: &mut Vec<BrowserEntry>,
    seen: &mut BTreeSet<String>,
    current_dir: &Path,
    path: &Path,
    kind: BrowserEntryKind,
) {
    if !path.is_dir() || same_path(path, current_dir) {
        return;
    }
    let identity = path_identity(path);
    if !seen.insert(identity) {
        return;
    }
    entries.push(BrowserEntry {
        path: path.to_path_buf(),
        name: path.to_string_lossy().into_owned(),
        kind,
    });
}

fn path_identity(path: &Path) -> String {
    if cfg!(windows) {
        path.to_string_lossy().replace('/', "\\").to_lowercase()
    } else {
        path.to_string_lossy().into_owned()
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_identity(left) == path_identity(right)
}

#[cfg(windows)]
fn system_roots() -> Vec<PathBuf> {
    (b'A'..=b'Z')
        .map(|letter| PathBuf::from(format!("{}:\\", char::from(letter))))
        .filter(|path| path.is_dir())
        .collect()
}

#[cfg(not(windows))]
fn system_roots() -> Vec<PathBuf> {
    vec![PathBuf::from("/")]
}

fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePaletteAction {
    Navigated,
    Attach(Vec<PathBuf>),
    None,
}

#[derive(Debug, Clone)]
pub struct PaletteUiState {
    mode: PaletteMode,
    dialog: DialogState<()>,
    picker: ListPickerState,
    focus: FocusManager<PaletteFocus>,
    clicks: ClickRegionRegistry<PaletteHit>,
    query: String,
    file_browser: Option<FileBrowserState>,
}

impl PaletteUiState {
    #[must_use]
    pub fn new() -> Self {
        let mut focus = FocusManager::new();
        focus.register(PaletteFocus::Close);
        focus.register(PaletteFocus::Primary);
        focus.set(PaletteFocus::Primary);
        Self {
            mode: PaletteMode::Closed,
            dialog: DialogState::new(()),
            picker: ListPickerState::new(0),
            focus,
            clicks: ClickRegionRegistry::new(),
            query: String::new(),
            file_browser: None,
        }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        !matches!(self.mode, PaletteMode::Closed)
    }

    #[must_use]
    pub const fn mode(&self) -> PaletteMode {
        self.mode
    }

    #[must_use]
    pub fn query(&self) -> &str {
        &self.query
    }

    #[must_use]
    pub const fn selected_index(&self) -> usize {
        self.picker.selected_index
    }

    #[must_use]
    pub fn focused(&self) -> Option<PaletteFocus> {
        self.focus.current().copied()
    }

    pub fn begin_frame(&mut self) {
        self.clicks.clear();
    }

    pub fn open(&mut self, mode: PaletteMode, total: usize) {
        self.mode = mode;
        self.query.clear();
        self.picker.set_total(total);
        self.picker.select_first();
        self.picker.scroll = 0;
        self.focus.set(PaletteFocus::Primary);
        self.dialog.show();
        if mode != PaletteMode::Files {
            self.file_browser = None;
        }
    }

    pub fn open_files(&mut self, workspace_root: &Path) -> io::Result<()> {
        let browser = FileBrowserState::open(workspace_root)?;
        let total = browser.entries.len();
        self.open(PaletteMode::Files, total);
        self.file_browser = Some(browser);
        Ok(())
    }

    pub fn close(&mut self) {
        self.mode = PaletteMode::Closed;
        self.query.clear();
        self.dialog.hide();
        self.clicks.clear();
        self.file_browser = None;
    }

    pub fn set_total(&mut self, total: usize) {
        self.picker.set_total(total);
    }

    pub fn select(&mut self, index: usize) {
        self.picker.select(index);
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

    pub fn focus(&mut self, focus: PaletteFocus) {
        self.focus.set(focus);
    }

    pub fn push_query(&mut self, character: char) {
        if !character.is_control()
            && self.query.len().saturating_add(character.len_utf8()) <= MAX_QUERY_BYTES
        {
            self.query.push(character);
            self.picker.select_first();
        }
    }

    pub fn push_query_text(&mut self, text: &str) {
        for character in text.chars() {
            if character.is_control() {
                continue;
            }
            if self.query.len().saturating_add(character.len_utf8()) > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.picker.select_first();
    }

    pub fn pop_query(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.picker.select_first();
        }
    }

    #[must_use]
    pub fn visible_file_count(&self) -> usize {
        self.file_browser
            .as_ref()
            .map_or(0, |browser| browser.visible_indices(&self.query).len())
    }

    #[must_use]
    pub fn visible_file_paths(&self) -> Vec<PathBuf> {
        let Some(browser) = &self.file_browser else {
            return Vec::new();
        };
        browser
            .visible_indices(&self.query)
            .into_iter()
            .filter_map(|index| browser.entries.get(index).map(|entry| entry.path.clone()))
            .collect()
    }

    pub fn navigate_files_to(&mut self, path: &Path) -> io::Result<()> {
        let Some(browser) = &mut self.file_browser else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file browser is not open",
            ));
        };
        browser.navigate_to(path.to_path_buf())?;
        self.query.clear();
        self.picker.set_total(browser.entries.len());
        self.picker.select_first();
        self.picker.scroll = 0;
        Ok(())
    }

    pub fn navigate_files_up(&mut self) -> io::Result<()> {
        let parent = self
            .file_browser
            .as_ref()
            .and_then(|browser| browser.current_dir.parent().map(Path::to_path_buf));
        match parent {
            Some(parent) => self.navigate_files_to(&parent),
            None => Ok(()),
        }
    }

    pub fn toggle_current_file(&mut self) {
        let Some(browser) = &mut self.file_browser else {
            return;
        };
        let indices = browser.visible_indices(&self.query);
        let Some(entry) = indices
            .get(self.picker.selected_index)
            .and_then(|index| browser.entries.get(*index))
        else {
            return;
        };
        if matches!(entry.kind, BrowserEntryKind::File { .. }) {
            let path = entry.path.clone();
            if !browser.selected_files.remove(&path) {
                browser.selected_files.insert(path);
            }
        }
    }

    pub fn activate_current_file_entry(&mut self) -> io::Result<FilePaletteAction> {
        let entry = self.current_browser_entry().cloned();
        let Some(entry) = entry else {
            return Ok(FilePaletteAction::None);
        };
        if entry.is_directory() {
            self.navigate_files_to(&entry.path)?;
            return Ok(FilePaletteAction::Navigated);
        }
        let selected = self.selected_or_current_files();
        Ok(FilePaletteAction::Attach(selected))
    }

    #[must_use]
    pub fn selected_or_current_files(&self) -> Vec<PathBuf> {
        let Some(browser) = &self.file_browser else {
            return Vec::new();
        };
        if !browser.selected_files.is_empty() {
            return browser.selected_files.iter().cloned().collect();
        }
        self.current_browser_entry()
            .filter(|entry| !entry.is_directory())
            .map(|entry| vec![entry.path.clone()])
            .unwrap_or_default()
    }

    fn current_browser_entry(&self) -> Option<&BrowserEntry> {
        let browser = self.file_browser.as_ref()?;
        browser
            .visible_indices(&self.query)
            .get(self.picker.selected_index)
            .and_then(|index| browser.entries.get(*index))
    }

    #[must_use]
    pub fn clicked(&self, column: u16, row: u16) -> Option<PaletteHit> {
        self.clicks.handle_click(column, row).copied()
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, custom_commands: &[CustomCommandSummary]) {
        if !self.is_open() {
            return;
        }
        let mode = self.mode;
        let query = self.query.to_lowercase();
        let command_matches = command_matches(custom_commands, &query);
        let file_matches = self
            .file_browser
            .as_ref()
            .map(|browser| browser.visible_indices(&query))
            .unwrap_or_default();
        let labels = match mode {
            PaletteMode::Commands => command_matches
                .iter()
                .map(|command| {
                    if command.hint.is_empty() {
                        sanitize_for_display(&command.label)
                    } else {
                        sanitize_for_display(&format!("{}  • {}", command.label, command.hint))
                    }
                })
                .collect::<Vec<_>>(),
            PaletteMode::Files => file_matches
                .iter()
                .filter_map(|index| {
                    let browser = self.file_browser.as_ref()?;
                    let entry = browser.entries.get(*index)?;
                    Some(sanitize_for_display(
                        &entry.label(browser.selected_files.contains(&entry.path)),
                    ))
                })
                .collect::<Vec<_>>(),
            PaletteMode::Closed => Vec::new(),
        };
        self.picker.set_total(labels.len());
        let focused = self.focus.current().copied();
        let title = match mode {
            PaletteMode::Commands => text(Text::CommandPalette).to_owned(),
            PaletteMode::Files => self.file_browser.as_ref().map_or_else(
                || text(Text::AttachComputerFile).to_owned(),
                |browser| {
                    let current_dir =
                        sanitize_for_display(browser.current_dir.to_string_lossy().as_ref());
                    format!(
                        "{} · {} · {} {}",
                        text(Text::AttachComputerFile),
                        current_dir,
                        browser.selected_files.len(),
                        text(Text::SelectedPrefix)
                    )
                },
            ),
            PaletteMode::Closed => text(Text::Palette).to_owned(),
        };
        let config = DialogConfig::new(&title)
            .width_percent(76)
            .height_percent(66)
            .min_size(58, 15)
            .max_size(140, 44)
            .border_color(Color::Cyan)
            .focused_border_color(Color::LightCyan)
            .close_on_escape(false)
            .close_on_outside_click(false)
            .no_buttons();
        let query_display = sanitize_for_display(&self.query);
        let file_selection_count = self
            .file_browser
            .as_ref()
            .map_or(0, |browser| browser.selected_files.len());
        let file_current_is_directory = self
            .current_browser_entry()
            .is_some_and(BrowserEntry::is_directory);
        let picker = &mut self.picker;
        let clicks = &mut self.clicks;
        let mut popup = PopupDialog::new(&config, &mut self.dialog, |frame, area, _| {
            draw_palette_body(
                frame,
                area,
                mode,
                &query_display,
                &labels,
                picker,
                focused,
                file_selection_count,
                file_current_is_directory,
                clicks,
            );
        });
        popup.render(frame);
    }
}

impl Default for PaletteUiState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_palette_body(
    frame: &mut Frame<'_>,
    area: Rect,
    mode: PaletteMode,
    query: &str,
    labels: &[String],
    picker: &mut ListPickerState,
    focused: Option<PaletteFocus>,
    file_selection_count: usize,
    file_current_is_directory: bool,
    clicks: &mut ClickRegionRegistry<PaletteHit>,
) {
    let chunks = Layout::vertical([
        Constraint::Length(4),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .split(area);
    let prefix = match mode {
        PaletteMode::Commands => "/",
        PaletteMode::Files => "@",
        PaletteMode::Closed => "",
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::LightCyan)),
                Span::styled(
                    if query.is_empty() {
                        text(Text::TypeToFilter)
                    } else {
                        query
                    },
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(if mode == PaletteMode::Files {
                text(Text::FileBrowserNavigationHint)
            } else {
                text(Text::PaletteNavigationHint)
            }),
        ])
        .block(Block::default().borders(Borders::ALL))
        .wrap(Wrap { trim: false }),
        chunks[0],
    );
    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} {} ",
        labels.len(),
        text(Text::MatchCount)
    ));
    let inner = block.inner(chunks[1]);
    frame.render_widget(block, chunks[1]);
    let viewport = usize::from(inner.height);
    picker.ensure_visible(viewport);
    let list_style = ListPickerStyle::bracket().bordered(false);
    let label_width =
        usize::from(inner.width).saturating_sub(UnicodeWidthStr::width(list_style.indicator));
    let fitted_labels = labels
        .iter()
        .map(|label| truncate_to_width(label, label_width))
        .collect::<Vec<_>>();
    frame.render_widget(
        ListPicker::new(&fitted_labels, picker).style(list_style),
        inner,
    );
    for visible_row in 0..viewport {
        let index = usize::from(picker.scroll).saturating_add(visible_row);
        if index >= labels.len() {
            break;
        }
        clicks.register(
            Rect::new(
                inner.x,
                inner.y.saturating_add(visible_row as u16),
                inner.width,
                1,
            ),
            PaletteHit::Item(index),
        );
    }
    let buttons = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(20),
        Constraint::Length(2),
        Constraint::Length(28),
        Constraint::Fill(1),
    ])
    .split(chunks[2]);
    let mut close_state = ButtonState::enabled();
    close_state.set_focused(focused == Some(PaletteFocus::Close));
    let close = Button::new(text(Text::Close), &close_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let close_region = close.render_stateful(buttons[1], frame.buffer_mut());
    clicks.register(close_region.area, PaletteHit::Close);

    let mut primary_state = if labels.is_empty() {
        ButtonState::disabled()
    } else {
        ButtonState::enabled()
    };
    primary_state.set_focused(focused == Some(PaletteFocus::Primary));
    let primary_label = match mode {
        PaletteMode::Commands => text(Text::RunSelectedAction),
        PaletteMode::Files if file_selection_count == 0 && file_current_is_directory => {
            text(Text::OpenLabel)
        }
        PaletteMode::Files => text(Text::AttachSelectedFile),
        PaletteMode::Closed => text(Text::Select),
    };
    let primary = Button::new(primary_label, &primary_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::primary());
    let primary_region = primary.render_stateful(buttons[3], frame.buffer_mut());
    if !labels.is_empty() {
        clicks.register(primary_region.area, PaletteHit::Primary);
    }
}

fn truncate_to_width(value: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(value) <= max_width {
        return value.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let content_width = max_width.saturating_sub(1);
    let mut width = 0_usize;
    let mut output = String::new();
    for grapheme in value.graphemes(true) {
        let next = UnicodeWidthStr::width(grapheme);
        if width.saturating_add(next) > content_width {
            break;
        }
        output.push_str(grapheme);
        width = width.saturating_add(next);
    }
    output.push('…');
    output
}

#[must_use]
pub fn command_matches(
    custom_commands: &[CustomCommandSummary],
    query: &str,
) -> Vec<PaletteCommandMatch> {
    let mut matches = COMMANDS
        .iter()
        .filter(|command| {
            let label = localized_command_label(command);
            let hint = localized_command_hint(command);
            query.is_empty()
                || command.id.contains(query)
                || label.to_lowercase().contains(query)
                || hint.to_lowercase().contains(query)
        })
        .map(|command| PaletteCommandMatch {
            selection: PaletteCommandSelection::BuiltIn(command.id),
            label: localized_command_label(command).to_owned(),
            hint: localized_command_hint(command).to_owned(),
        })
        .collect::<Vec<_>>();
    matches.extend(custom_commands.iter().filter_map(|command| {
        let source = match command.source {
            AutomationSource::User => text(Text::UserSource),
            AutomationSource::Project => text(Text::ProjectSource),
        };
        let searchable = format!(
            "{} {} {} {} {}",
            command.id, command.name, command.description, command.argument_hint, source
        )
        .to_lowercase();
        (query.is_empty() || searchable.contains(query)).then(|| PaletteCommandMatch {
            selection: PaletteCommandSelection::Custom(command.id.clone()),
            label: format!("/{}  {}", command.id, command.name),
            hint: format!("{} · {source}", command.description),
        })
    }));
    matches
}

fn localized_command_label(command: &PaletteCommand) -> &'static str {
    match command.id {
        "new_session" => text(Text::NewSession),
        "sessions" => text(Text::SessionManager),
        "rewind" => text(Text::CheckpointRewind),
        "runtime" => text(Text::RuntimeSettingsMenu),
        "language" => text(Text::InterfaceLanguage),
        "rerun_setup" => text(Text::RerunSetup),
        "mcp" => text(Text::McpConnections),
        "lsp" => text(Text::LanguageIntelligence),
        "code_index" => text(Text::RepositoryIntelligence),
        "privacy" => text(Text::PrivacyShield),
        "permissions" => text(Text::SessionPermissions),
        "auto_approval" => text(Text::AutoApprovalCenter),
        "usage" => text(Text::TokenUsageCost),
        "notifications" => text(Text::Notifications),
        "reviews" => text(Text::StructuredCodeReviews),
        "side_chat" => text(Text::SideQuestion),
        "follow_ups" => text(Text::QueueFollowUps),
        "modes" => text(Text::WorkModeMenu),
        "instructions" => text(Text::RepositoryInstructions),
        "skills" => text(Text::AgentSkills),
        "plugins" => text(Text::PluginManager),
        "automation" => text(Text::Automation),
        "tab_chat" => text(Text::OpenChat),
        "tab_activity" => text(Text::OpenActivity),
        "tab_diff" => text(Text::OpenDiff),
        "tab_plan" => text(Text::OpenPlan),
        "tab_agents" => text(Text::OpenAgents),
        "tab_terminal" => text(Text::OpenTerminal),
        "terminal_new" => text(Text::NewTerminal),
        "terminal_stop" => text(Text::StopActiveTerminal),
        "terminal_close" => text(Text::CloseActiveTerminal),
        "toggle_left" => text(Text::ToggleWorkspaceSidebar),
        "toggle_right" => text(Text::ToggleStatusSidebar),
        "jump_latest" => text(Text::JumpLatest),
        "shortcuts" => text(Text::KeyboardShortcuts),
        _ => text(Text::SelectedItemUnavailable),
    }
}

fn localized_command_hint(command: &PaletteCommand) -> &'static str {
    match command.id {
        "new_session" => text(Text::ConversationReset),
        "sessions" => text(Text::ManagePersistentSession),
        "rewind" => text(Text::RewindNavigationHelp),
        "runtime" => text(Text::TrustedDeploymentHelp),
        "language" => text(Text::InterfaceLanguage),
        "rerun_setup" => text(Text::SetupNextLaunch),
        "mcp" => text(Text::McpFiniteRetryHelp),
        "lsp" => text(Text::LspBoundedHelp),
        "code_index" => text(Text::LocalIndexSearchHelp),
        "privacy" => text(Text::PrivacySourcesHelp),
        "permissions" => text(Text::SessionGrantHelp),
        "auto_approval" => text(Text::AutoApprovalCenter),
        "usage" => text(Text::ExactUsagePricingHelp),
        "notifications" => text(Text::NotificationSnapshotHelp),
        "reviews" => text(Text::ReviewIdleDecisionHelp),
        "side_chat" => text(Text::SideQuestionsSeparate),
        "follow_ups" => text(Text::QueueSteerHelp),
        "modes" => text(Text::IndependentModesHelp),
        "instructions" => text(Text::NestedScopesHelp),
        "skills" => text(Text::SkillMetadataHelp),
        "plugins" => text(Text::PluginPackageHelp),
        "automation" => text(Text::AutomationHookSafetyHelp),
        "tab_chat" => text(Text::ChatFollowing),
        "tab_activity" => text(Text::ActivityHint),
        "tab_diff" => text(Text::ReviewPatchHunks),
        "tab_plan" => text(Text::PlanDurableHelp),
        "tab_agents" => text(Text::DelegationStartHelp),
        "tab_terminal" => text(Text::TerminalControlsHelp),
        "terminal_new" => text(Text::OpeningTerminalNotice),
        "terminal_stop" => text(Text::StoppingTerminalNotice),
        "terminal_close" => text(Text::ClosingTerminalNotice),
        "toggle_left" => text(Text::WorkspaceFiles),
        "toggle_right" => text(Text::Status),
        "jump_latest" => text(Text::FollowingLatestOutput),
        "shortcuts" => text(Text::ShortcutSummary),
        _ => text(Text::SelectedItemUnavailable),
    }
}

#[must_use]
pub fn file_matches(files: &[String], query: &str) -> Vec<usize> {
    files
        .iter()
        .enumerate()
        .filter(|(_, path)| query.is_empty() || path.to_lowercase().contains(query))
        .take(1_000)
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::PathBuf};

    use ratatui::{Terminal, backend::TestBackend};

    use crate::agent::automation::{AutomationSource, CustomCommandSummary};

    use super::{
        PaletteCommandSelection, PaletteFocus, PaletteHit, PaletteMode, PaletteUiState,
        command_matches, file_matches,
    };

    #[test]
    fn palette_filters_commands_and_full_workspace_paths() {
        let matches = command_matches(&[], "rewind");
        assert!(
            matches
                .iter()
                .any(|entry| { entry.selection == PaletteCommandSelection::BuiltIn("rewind") })
        );
        let files = vec!["src/ui/app.rs".to_owned(), "tests/ui_tests.rs".to_owned()];
        assert_eq!(file_matches(&files, "ui"), vec![0, 1]);
        assert_eq!(file_matches(&files, "tests/"), vec![1]);
    }

    #[test]
    fn every_builtin_command_has_localized_copy() {
        for command in super::COMMANDS {
            assert!(!super::localized_command_label(&command).trim().is_empty());
            assert!(!super::localized_command_hint(&command).trim().is_empty());
        }
    }

    #[test]
    fn palette_searches_custom_commands_and_keeps_their_identity() {
        let custom = vec![CustomCommandSummary {
            id: "review".to_owned(),
            name: "Security review".to_owned(),
            description: "Inspect a change".to_owned(),
            source: AutomationSource::Project,
            source_path: PathBuf::from("review.command.toml"),
            argument_hint: "<path>".to_owned(),
            requires_arguments: true,
        }];
        let matches = command_matches(&custom, "security");
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].selection,
            PaletteCommandSelection::Custom("review".to_owned())
        );
    }

    #[test]
    fn opening_palette_never_executes_selection() {
        let mut palette = PaletteUiState::new();
        palette.open(PaletteMode::Commands, 10);
        assert!(palette.is_open());
        assert_eq!(palette.selected_index(), 0);
        palette.close();
        assert!(!palette.is_open());
    }

    #[test]
    fn visible_palette_actions_have_mouse_and_focus_paths() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut palette = PaletteUiState::new();
        palette.open(PaletteMode::Commands, super::COMMANDS.len());
        let mut terminal = Terminal::new(TestBackend::new(100, 30))?;
        terminal.draw(|frame| {
            palette.begin_frame();
            palette.draw(frame, &[]);
        })?;

        let mut hits = HashSet::new();
        for row in 0..30 {
            for column in 0..100 {
                if let Some(hit) = palette.clicked(column, row) {
                    hits.insert(hit);
                }
            }
        }
        assert!(hits.contains(&PaletteHit::Item(0)));
        assert!(hits.contains(&PaletteHit::Close));
        assert!(hits.contains(&PaletteHit::Primary));

        assert_eq!(palette.focused(), Some(PaletteFocus::Primary));
        palette.next_focus();
        assert_eq!(palette.focused(), Some(PaletteFocus::Close));
        palette.previous_focus();
        assert_eq!(palette.focused(), Some(PaletteFocus::Primary));
        Ok(())
    }

    #[test]
    fn query_editor_respects_its_utf8_byte_limit() {
        let mut palette = PaletteUiState::new();
        palette.open(PaletteMode::Commands, 10);
        for _ in 0..4_095 {
            palette.push_query('a');
        }

        palette.push_query('é');

        assert_eq!(palette.query().len(), 4_095);
    }

    #[test]
    fn query_backspace_removes_one_grapheme() {
        let mut palette = PaletteUiState::new();
        palette.open(PaletteMode::Commands, 10);
        palette.push_query('e');
        palette.push_query('\u{301}');

        palette.pop_query();

        assert!(palette.query().is_empty());
    }

    #[test]
    fn changing_query_resets_selection_to_the_first_match() {
        let mut palette = PaletteUiState::new();
        palette.open(PaletteMode::Commands, 10);
        palette.select(7);

        palette.push_query('r');

        assert_eq!(palette.selected_index(), 0);
    }
}
