use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};
use ratatui_interact::{
    components::{
        ContextMenu, ContextMenuAction, ContextMenuItem, ContextMenuState, Menu, MenuBar,
        MenuBarAction, MenuBarClickTarget, MenuBarItem, MenuBarState, Tab, TabPosition, TabView,
        TabViewAction, TabViewState, TabViewStyle, handle_context_menu_key,
        handle_context_menu_mouse, handle_menu_bar_key, handle_menu_bar_mouse, handle_tab_view_key,
        handle_tab_view_mouse,
    },
    traits::{ClickRegion, ClickRegionRegistry as RawClickRegionRegistry},
};

use super::{
    actions::ClickRegionRegistry,
    i18n::{Text, text},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellTab {
    Chat,
    Activity,
    Diff,
    Plan,
    Agents,
    Terminal,
}

impl ShellTab {
    pub const ALL: [Self; 6] = [
        Self::Chat,
        Self::Activity,
        Self::Diff,
        Self::Plan,
        Self::Agents,
        Self::Terminal,
    ];

    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Activity => 1,
            Self::Diff => 2,
            Self::Plan => 3,
            Self::Agents => 4,
            Self::Terminal => 5,
        }
    }

    #[must_use]
    pub const fn from_index(index: usize) -> Self {
        match index {
            1 => Self::Activity,
            2 => Self::Diff,
            3 => Self::Plan,
            4 => Self::Agents,
            5 => Self::Terminal,
            _ => Self::Chat,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellHit {
    Session(usize),
    File(usize),
    RemoveAttachment(usize),
    Tool(u64),
    JumpLatest,
    RetryFailedTurn,
    AbortFailedTurn,
    PauseTurn,
    ResumePausedTurn,
    AbortPausedTurn,
    MascotFeed,
    MascotWake,
    McpManager,
    RuntimeManager,
    ModesManager,
    FollowUps,
    SideChat,
    UsageManager,
    ReviewManager,
    NotificationCenter,
    InstructionsManager,
    SkillsManager,
    PluginsManager,
    PrivacyShield,
    ShellPermissions,
    AutoApprovalCenter,
}

#[derive(Debug, Clone)]
pub struct ShellUiState {
    menu: MenuBarState,
    menu_bar_area: Rect,
    menu_dropdown_area: Option<Rect>,
    menu_regions: Vec<ClickRegion<MenuBarClickTarget>>,
    tabs: TabViewState,
    tab_regions: RawClickRegionRegistry<TabViewAction>,
    hits: ClickRegionRegistry<ShellHit>,
    tool_menu: ContextMenuState,
    tool_menu_action_id: Option<u64>,
    tool_menu_area: Rect,
    tool_menu_regions: Vec<ClickRegion<ContextMenuAction>>,
    pub show_left_sidebar: bool,
    pub show_right_sidebar: bool,
    pub follow_output: bool,
    retry_focused: bool,
}

impl ShellUiState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            menu: MenuBarState::new(),
            menu_bar_area: Rect::default(),
            menu_dropdown_area: None,
            menu_regions: Vec::new(),
            tabs: TabViewState::new(ShellTab::ALL.len()),
            tab_regions: RawClickRegionRegistry::new(),
            hits: ClickRegionRegistry::new(),
            tool_menu: ContextMenuState::new(),
            tool_menu_action_id: None,
            tool_menu_area: Rect::default(),
            tool_menu_regions: Vec::new(),
            show_left_sidebar: true,
            show_right_sidebar: true,
            follow_output: true,
            retry_focused: true,
        }
    }

    pub fn begin_frame(&mut self) {
        self.tab_regions.clear();
        self.hits.clear();
        self.menu_regions.clear();
        self.tool_menu_regions.clear();
    }

    #[must_use]
    pub const fn active_tab(&self) -> ShellTab {
        ShellTab::from_index(self.tabs.selected_index)
    }

    pub fn select_tab(&mut self, tab: ShellTab) {
        self.tabs.select(tab.index());
    }

    #[must_use]
    pub const fn menu_is_open(&self) -> bool {
        self.menu.is_open
    }

    #[must_use]
    pub const fn tool_menu_is_open(&self) -> bool {
        self.tool_menu.is_open
    }

    pub fn close_menu(&mut self) {
        self.menu.close_menu();
    }

    pub fn open_menu(&mut self) {
        self.tool_menu.close();
        self.menu.open_menu(self.menu.active_menu);
    }

    pub fn open_tool_menu(&mut self, action_id: u64, column: u16, row: u16) {
        self.menu.close_menu();
        self.tool_menu_action_id = Some(action_id);
        self.tool_menu.open_at(column, row);
        self.tool_menu.highlight_first(&tool_menu_items());
    }

    pub fn next_tab(&mut self) {
        if self.tabs.selected_index + 1 >= self.tabs.total_tabs {
            self.tabs.select_first();
        } else {
            self.tabs.select_next();
        }
    }

    pub fn previous_tab(&mut self) {
        if self.tabs.selected_index == 0 {
            self.tabs.select_last();
        } else {
            self.tabs.select_prev();
        }
    }

    pub fn toggle_failed_action_focus(&mut self) {
        self.retry_focused = !self.retry_focused;
    }

    #[must_use]
    pub const fn retry_is_focused(&self) -> bool {
        self.retry_focused
    }

    pub fn draw_menu(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        idle: bool,
        mascot_enabled: bool,
        show_thinking: bool,
        show_tool_activity: bool,
    ) {
        let definitions = menus(idle, mascot_enabled, show_thinking, show_tool_activity);
        let (bar, dropdown, regions) =
            MenuBar::new(&definitions, &self.menu).render_stateful(frame, area);
        self.menu_bar_area = bar;
        self.menu_dropdown_area = dropdown;
        self.menu_regions = regions;
    }

    pub fn draw_tabs(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let tabs = [
            Tab::new(text(Text::Chat)).icon("●"),
            Tab::new(text(Text::Activity)).icon("◆"),
            Tab::new(text(Text::Diff)).icon("±"),
            Tab::new(text(Text::Plan)).icon("✓"),
            Tab::new(text(Text::Agents)).icon("◎"),
        ];
        let mut tabs = Vec::from(tabs);
        tabs.push(Tab::new(text(Text::Terminal)).icon(">_"));
        TabView::new(&tabs, &self.tabs)
            .style(TabViewStyle::minimal())
            .content(|_, _, _| {})
            .render_with_registry(area, frame.buffer_mut(), &mut self.tab_regions);
    }

    pub fn draw_tool_menu(&mut self, frame: &mut Frame<'_>, screen: Rect) {
        if !self.tool_menu.is_open {
            return;
        }
        let items = tool_menu_items();
        let (area, regions) =
            ContextMenu::new(&items, &self.tool_menu).render_stateful(frame, screen);
        self.tool_menu_area = area;
        self.tool_menu_regions = regions;
    }

    #[must_use]
    pub fn content_area(area: Rect) -> Rect {
        Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        )
    }

    pub fn register_hit(&mut self, area: Rect, hit: ShellHit) {
        self.hits.register(area, hit);
    }

    #[must_use]
    pub fn hit(&self, column: u16, row: u16) -> Option<ShellHit> {
        self.hits.handle_click(column, row).copied()
    }

    pub fn handle_menu_key(
        &mut self,
        key: &KeyEvent,
        idle: bool,
        mascot_enabled: bool,
        show_thinking: bool,
        show_tool_activity: bool,
    ) -> Option<String> {
        match handle_menu_bar_key(
            key,
            &mut self.menu,
            &menus(idle, mascot_enabled, show_thinking, show_tool_activity),
        ) {
            Some(MenuBarAction::ItemSelect(action)) => Some(action),
            Some(_) | None => None,
        }
    }

    pub fn handle_menu_mouse(
        &mut self,
        mouse: &MouseEvent,
        idle: bool,
        mascot_enabled: bool,
        show_thinking: bool,
        show_tool_activity: bool,
    ) -> Option<String> {
        match handle_menu_bar_mouse(
            mouse,
            &mut self.menu,
            self.menu_bar_area,
            self.menu_dropdown_area,
            &self.menu_regions,
            &menus(idle, mascot_enabled, show_thinking, show_tool_activity),
        ) {
            Some(MenuBarAction::ItemSelect(action)) => Some(action),
            Some(_) | None => None,
        }
    }

    pub fn handle_tool_menu_key(&mut self, key: &KeyEvent) -> Option<(u64, String)> {
        let action_id = self.tool_menu_action_id?;
        match handle_context_menu_key(key, &mut self.tool_menu, &tool_menu_items()) {
            Some(ContextMenuAction::Select(action)) => Some((action_id, action)),
            Some(_) | None => None,
        }
    }

    pub fn handle_tool_menu_mouse(&mut self, mouse: &MouseEvent) -> Option<(u64, String)> {
        let action_id = self.tool_menu_action_id?;
        match handle_context_menu_mouse(
            mouse,
            &mut self.tool_menu,
            self.tool_menu_area,
            &self.tool_menu_regions,
        ) {
            Some(ContextMenuAction::Select(action)) => Some((action_id, action)),
            Some(_) | None => None,
        }
    }

    pub fn handle_tab_key(&mut self, key: &KeyEvent) -> bool {
        handle_tab_view_key(&mut self.tabs, key, TabPosition::Top)
    }

    pub fn handle_tab_mouse(&mut self, mouse: &MouseEvent) -> bool {
        handle_tab_view_mouse(&mut self.tabs, &self.tab_regions, mouse).is_some()
    }
}

fn tool_menu_items() -> Vec<ContextMenuItem> {
    vec![
        ContextMenuItem::action("toggle_details", text(Text::ExpandCollapseDetails))
            .shortcut("Enter"),
        ContextMenuItem::action("open_chat", text(Text::ShowInChat)),
        ContextMenuItem::action("open_diff", text(Text::ShowInDiff)),
        ContextMenuItem::separator(),
        ContextMenuItem::action("mention_output", text(Text::MentionTool)),
    ]
}

impl Default for ShellUiState {
    fn default() -> Self {
        Self::new()
    }
}

fn menus(
    idle: bool,
    mascot_enabled: bool,
    show_thinking: bool,
    show_tool_activity: bool,
) -> Vec<Menu> {
    vec![
        Menu::new(text(Text::Session)).items(vec![
            MenuBarItem::action("new_session", text(Text::NewSession))
                .shortcut("Ctrl+N")
                .enabled(idle),
            MenuBarItem::action("sessions", text(Text::SessionManager))
                .shortcut("Ctrl+O")
                .enabled(idle),
            MenuBarItem::action("rewind", text(Text::CheckpointRewind))
                .shortcut("Ctrl+Z")
                .enabled(idle),
            MenuBarItem::separator(),
            MenuBarItem::action("quit", text(Text::Quit)).shortcut("Ctrl+Q"),
        ]),
        Menu::new(text(Text::View)).items(vec![
            MenuBarItem::action("tab_chat", text(Text::Chat)),
            MenuBarItem::action("tab_activity", text(Text::Activity)),
            MenuBarItem::action("tab_diff", text(Text::Diff)),
            MenuBarItem::action("tab_plan", text(Text::Plan)),
            MenuBarItem::action("tab_agents", text(Text::Agents)),
            MenuBarItem::action("tab_terminal", text(Text::Terminal)).shortcut("Ctrl+Shift+T"),
            MenuBarItem::separator(),
            MenuBarItem::action("toggle_left", text(Text::ToggleWorkspaceSidebar)),
            MenuBarItem::action("toggle_right", text(Text::ToggleStatusSidebar)),
            MenuBarItem::action("jump_latest", text(Text::JumpLatest)).shortcut("End"),
            MenuBarItem::separator(),
            MenuBarItem::action(
                "toggle_pixel",
                if mascot_enabled {
                    text(Text::ShowPixelOn)
                } else {
                    text(Text::ShowPixelOff)
                },
            ),
            MenuBarItem::action(
                "toggle_thinking",
                if show_thinking {
                    text(Text::ShowThinkingOn)
                } else {
                    text(Text::ShowThinkingOff)
                },
            ),
            MenuBarItem::action(
                "toggle_tool_activity",
                if show_tool_activity {
                    text(Text::ShowToolsOn)
                } else {
                    text(Text::ShowToolsOff)
                },
            ),
        ]),
        Menu::new(text(Text::Agent)).items(vec![
            MenuBarItem::action("runtime", text(Text::RuntimeSettingsMenu))
                .shortcut("Ctrl+M")
                .enabled(idle),
            MenuBarItem::action("mcp", text(Text::McpConnections))
                .shortcut("Ctrl+K")
                .enabled(idle),
            MenuBarItem::action("lsp", text(Text::LanguageIntelligence))
                .shortcut("Ctrl+L")
                .enabled(idle),
            MenuBarItem::action("code_index", text(Text::RepositoryIntelligence))
                .shortcut("Ctrl+B")
                .enabled(idle),
            MenuBarItem::action("privacy", text(Text::PrivacyShield)).enabled(idle),
            MenuBarItem::action("permissions", text(Text::SessionPermissions)).enabled(idle),
            MenuBarItem::action("auto_approval", text(Text::AutoApprovalCenter)).enabled(idle),
            MenuBarItem::action("usage", text(Text::TokenUsageCost)),
            MenuBarItem::action("notifications", text(Text::Notifications)),
            MenuBarItem::action("github", text(Text::GithubPullRequests)).enabled(idle),
            MenuBarItem::action("reviews", text(Text::StructuredCodeReviews)),
            MenuBarItem::action("follow_ups", text(Text::QueueAndSteer)).shortcut("Ctrl+J"),
            MenuBarItem::action("side_chat", text(Text::SideQuestion)).shortcut("Ctrl+Y"),
            MenuBarItem::action("modes", text(Text::WorkModeMenu))
                .shortcut("Ctrl+G")
                .enabled(idle),
            MenuBarItem::action("instructions", text(Text::RepositoryInstructions)).enabled(idle),
            MenuBarItem::action("skills", text(Text::AgentSkills)).enabled(idle),
            MenuBarItem::action("plugins", text(Text::PluginManager)).enabled(idle),
            MenuBarItem::action("automation", text(Text::Automation)).enabled(idle),
            MenuBarItem::action("interrupt", text(Text::InterruptTurn)).enabled(!idle),
        ]),
        Menu::new(text(Text::Help)).items(vec![
            MenuBarItem::action("palette", text(Text::CommandPalette)).shortcut("/"),
            MenuBarItem::action("language", text(Text::InterfaceLanguage)),
            MenuBarItem::action("rerun_setup", text(Text::RerunSetup)).enabled(idle),
            MenuBarItem::action("shortcuts", text(Text::KeyboardShortcuts)),
        ]),
    ]
}

#[cfg(test)]
mod tests {
    use super::{ShellTab, ShellUiState};

    #[test]
    fn shell_has_six_stable_tabs_and_sidebar_toggles() {
        let mut shell = ShellUiState::new();
        shell.select_tab(ShellTab::Agents);
        assert_eq!(shell.active_tab(), ShellTab::Agents);
        assert!(shell.show_left_sidebar);
        assert!(shell.show_right_sidebar);
        assert!(shell.follow_output);
    }
}
