use super::{McpApprovalMode, McpPermissionConfig, McpTool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPermissionDecision {
    Allow,
    RequireApproval { reason: String },
    Deny { reason: String },
}

#[must_use]
pub fn evaluate_permission(
    permissions: &McpPermissionConfig,
    tool: &McpTool,
) -> McpPermissionDecision {
    if permissions.disabled_tools.contains(&tool.name) {
        return McpPermissionDecision::Deny {
            reason: format!(
                "{}::{} is explicitly disabled by the trusted MCP configuration",
                tool.server, tool.name
            ),
        };
    }
    if !permissions.enabled_tools.is_empty() && !permissions.enabled_tools.contains(&tool.name) {
        return McpPermissionDecision::Deny {
            reason: format!(
                "{}::{} is not present in enabled_tools",
                tool.server, tool.name
            ),
        };
    }

    match permissions.approval {
        McpApprovalMode::Never => McpPermissionDecision::Allow,
        McpApprovalMode::Always => McpPermissionDecision::RequireApproval {
            reason: permission_reason(tool, "server calls always require approval"),
        },
        McpApprovalMode::Writes
            if permissions.trusted_read_only_tools.contains(&tool.name)
                && tool.read_only_hint == Some(true)
                && tool.destructive_hint != Some(true) =>
        {
            McpPermissionDecision::Allow
        }
        McpApprovalMode::Writes => McpPermissionDecision::RequireApproval {
            reason: permission_reason(
                tool,
                "missing the two independent read-only signals (trusted config + server annotation)",
            ),
        },
    }
}

fn permission_reason(tool: &McpTool, reason: &str) -> String {
    format!(
        "{}::{} requires confirmation: {reason}",
        tool.server, tool.name
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn tool(read_only_hint: Option<bool>) -> McpTool {
        McpTool {
            server: "files".to_owned(),
            name: "read".to_owned(),
            function_name: "mcp__files__read".to_owned(),
            title: None,
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            read_only_hint,
            destructive_hint: None,
            open_world_hint: None,
        }
    }

    #[test]
    fn writes_mode_is_fail_closed_without_both_read_only_signals() {
        let mut config = McpPermissionConfig {
            approval: McpApprovalMode::Writes,
            ..McpPermissionConfig::default()
        };
        assert!(matches!(
            evaluate_permission(&config, &tool(Some(true))),
            McpPermissionDecision::RequireApproval { .. }
        ));
        config.trusted_read_only_tools = BTreeSet::from(["read".to_owned()]);
        assert_eq!(
            evaluate_permission(&config, &tool(Some(true))),
            McpPermissionDecision::Allow
        );
        assert!(matches!(
            evaluate_permission(&config, &tool(None)),
            McpPermissionDecision::RequireApproval { .. }
        ));
    }

    #[test]
    fn disabled_tool_wins_over_every_auto_approval_setting() {
        let mut config = McpPermissionConfig {
            approval: McpApprovalMode::Never,
            ..McpPermissionConfig::default()
        };
        config.disabled_tools.insert("read".to_owned());
        assert!(matches!(
            evaluate_permission(&config, &tool(Some(true))),
            McpPermissionDecision::Deny { .. }
        ));
    }
}
