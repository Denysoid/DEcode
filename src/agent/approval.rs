use serde::{Deserialize, Serialize};

/// Session-scoped auto-approval choices. Every capability is independent so
/// users can automate repetitive review without granting unrelated authority.
/// Hard denials and forced-confirm shell rules are enforced outside this
/// policy and can never be overridden by these flags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoApprovalPolicy {
    pub plans: bool,
    pub workspace_changes: bool,
    pub shell: bool,
    pub mcp_read_only: bool,
    pub mcp_mutating: bool,
    pub continuations: bool,
    pub subagent_shell: bool,
    pub subagent_changes: bool,
}

impl AutoApprovalPolicy {
    #[must_use]
    pub const fn all_enabled(self) -> bool {
        self.plans
            && self.workspace_changes
            && self.shell
            && self.mcp_read_only
            && self.mcp_mutating
            && self.continuations
            && self.subagent_shell
            && self.subagent_changes
    }

    pub fn set_all(&mut self, enabled: bool) {
        self.plans = enabled;
        self.workspace_changes = enabled;
        self.shell = enabled;
        self.mcp_read_only = enabled;
        self.mcp_mutating = enabled;
        self.continuations = enabled;
        self.subagent_shell = enabled;
        self.subagent_changes = enabled;
    }

    #[must_use]
    pub const fn enabled_count(self) -> usize {
        self.plans as usize
            + self.workspace_changes as usize
            + self.shell as usize
            + self.mcp_read_only as usize
            + self.mcp_mutating as usize
            + self.continuations as usize
            + self.subagent_shell as usize
            + self.subagent_changes as usize
    }
}

#[cfg(test)]
mod tests {
    use super::AutoApprovalPolicy;

    #[test]
    fn all_preset_is_reversible_and_complete() {
        let mut policy = AutoApprovalPolicy::default();
        policy.set_all(true);
        assert!(policy.all_enabled());
        assert_eq!(policy.enabled_count(), 8);
        policy.set_all(false);
        assert_eq!(policy, AutoApprovalPolicy::default());
    }

    #[test]
    fn older_session_json_defaults_new_policy_fields_off() -> Result<(), serde_json::Error> {
        let policy: AutoApprovalPolicy = serde_json::from_str("{}")?;
        assert_eq!(policy, AutoApprovalPolicy::default());
        Ok(())
    }
}
