use ratatui::layout::Rect;
use ratatui_interact::traits::ClickRegionRegistry as RawClickRegionRegistry;

/// Keyboard contract paired with every active mouse region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardPath {
    /// The action is reachable through focus navigation plus Enter/Space, or
    /// through an equivalent documented shortcut.
    FocusOrShortcut,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionBinding<T> {
    pub area: Rect,
    pub action: T,
    pub visible: bool,
    pub enabled: bool,
    pub keyboard: KeyboardPath,
}

/// Shared registry for active UI actions. Disabled controls are deliberately
/// not registered; every registered mouse target carries a keyboard contract.
#[derive(Debug, Clone)]
pub struct ClickRegionRegistry<T: Clone> {
    mouse: RawClickRegionRegistry<T>,
    bindings: Vec<ActionBinding<T>>,
}

impl<T: Clone> ClickRegionRegistry<T> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mouse: RawClickRegionRegistry::new(),
            bindings: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        self.mouse.clear();
        self.bindings.clear();
    }

    #[must_use]
    pub fn handle_click(&self, column: u16, row: u16) -> Option<&T> {
        self.mouse.handle_click(column, row)
    }

    #[must_use]
    pub fn bindings(&self) -> &[ActionBinding<T>] {
        &self.bindings
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.bindings
            .iter()
            .all(|binding| binding.area.width > 0 && binding.area.height > 0)
    }
}

impl<T: Clone> ClickRegionRegistry<T> {
    pub fn register(&mut self, area: Rect, action: T) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.mouse.register(area, action.clone());
        self.bindings.push(ActionBinding {
            area,
            action,
            visible: true,
            enabled: true,
            keyboard: KeyboardPath::FocusOrShortcut,
        });
    }
}

impl<T: Clone> Default for ClickRegionRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_action_always_has_mouse_area_and_keyboard_contract() {
        let mut registry = ClickRegionRegistry::new();
        registry.register(Rect::new(2, 3, 8, 1), "apply");

        assert!(registry.is_complete());
        assert_eq!(registry.handle_click(4, 3), Some(&"apply"));
        assert_eq!(
            registry.bindings()[0].keyboard,
            KeyboardPath::FocusOrShortcut
        );
    }

    #[test]
    fn clipped_action_is_not_registered_as_visible() {
        let mut registry = ClickRegionRegistry::new();
        registry.register(Rect::new(0, 0, 0, 1), "hidden");

        assert!(registry.bindings().is_empty());
        assert_eq!(registry.handle_click(0, 0), None);
    }

    #[test]
    fn every_button_module_uses_the_shared_action_registry() -> Result<(), std::io::Error> {
        let ui_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        for entry in std::fs::read_dir(ui_root)? {
            let path = entry?.path();
            if path.extension().and_then(|value| value.to_str()) != Some("rs")
                || matches!(
                    path.file_name().and_then(|value| value.to_str()),
                    Some("actions.rs" | "i18n.rs")
                )
            {
                continue;
            }
            let source = std::fs::read_to_string(&path)?;
            if !source.contains("Button::new") {
                continue;
            }
            assert!(
                source.contains("actions::ClickRegionRegistry") || source.contains("register_hit("),
                "{} renders buttons outside the shared action registry",
                path.display()
            );
            assert!(
                source.contains(".register(") || source.contains("register_hit("),
                "{} renders a button without registering its mouse target",
                path.display()
            );
        }
        Ok(())
    }

    #[test]
    fn every_button_surface_has_a_keyboard_dispatch_path() -> Result<(), std::io::Error> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui");
        let input = std::fs::read_to_string(root.join("input.rs"))?;
        let routes: &[(&str, &[&str])] = &[
            (
                "agents.rs",
                &["fn handle_agents_key", "fn handle_agent_editor_key"],
            ),
            ("approval_center.rs", &["fn handle_approval_center_key"]),
            ("automation.rs", &["fn handle_automation_key"]),
            ("code_index.rs", &["fn handle_code_index_key"]),
            (
                "confirm.rs",
                &["fn handle_confirmation_key", "fn handle_continuation_key"],
            ),
            (
                "connections.rs",
                &["fn handle_lsp_editor_key", "fn handle_mcp_editor_key"],
            ),
            ("followups.rs", &["fn handle_follow_up_key"]),
            ("github.rs", &["fn handle_github_key"]),
            ("instructions.rs", &["fn handle_instructions_key"]),
            ("language.rs", &["fn handle_language_key"]),
            ("lsp.rs", &["fn handle_lsp_key"]),
            ("mcp.rs", &["fn handle_mcp_key"]),
            (
                "modes.rs",
                &["fn handle_modes_key", "fn handle_plan_review_key"],
            ),
            ("notifications.rs", &["fn handle_notification_key"]),
            ("palette.rs", &["fn handle_palette_key"]),
            ("patch_review.rs", &["fn handle_patch_review_key"]),
            ("permissions.rs", &["fn handle_permission_key"]),
            ("plugins.rs", &["fn handle_plugin_key"]),
            ("privacy.rs", &["fn handle_privacy_key"]),
            ("render.rs", &["fn handle_shell_hit"]),
            ("review.rs", &["fn handle_review_key"]),
            ("rewind.rs", &["fn handle_rewind_key"]),
            ("runtime.rs", &["fn handle_runtime_key"]),
            ("sessions.rs", &["fn handle_session_key"]),
            ("side_chat.rs", &["fn handle_side_chat_key"]),
            ("skills.rs", &["fn handle_skills_key"]),
            ("terminal.rs", &["fn handle_terminal_key"]),
            ("usage.rs", &["fn handle_usage_key"]),
        ];

        for (file, handlers) in routes {
            let source = std::fs::read_to_string(root.join(file))?;
            assert!(
                source.contains("Button::new"),
                "{file} no longer renders a button"
            );
            assert!(
                source.contains(".register(") || source.contains("register_hit("),
                "{file} has no mouse action registration"
            );
            for handler in *handlers {
                assert!(
                    input.contains(handler),
                    "{file} has no keyboard dispatch path through {handler}"
                );
            }
        }

        let onboarding = std::fs::read_to_string(root.join("onboarding.rs"))?;
        assert!(onboarding.contains("Event::Key(key)"));
        assert!(onboarding.contains(".clicks.register("));
        Ok(())
    }
}
