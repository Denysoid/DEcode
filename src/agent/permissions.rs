use std::{collections::VecDeque, sync::Arc};

use crate::tools::{CommandDigest, ConfirmationDecision, ConfirmationReason};

pub const MAX_SESSION_SHELL_GRANTS: usize = 32;
pub const MAX_SESSION_SHELL_GRANT_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellApprovalDecision {
    Decline,
    RunOnce,
    TrustExactForSession,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellCommandGrant {
    pub id: u64,
    pub command: String,
    pub command_digest: CommandDigest,
    pub granted_turn_id: u64,
    pub granted_action_id: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellPermissionSnapshot {
    pub revision: u64,
    pub grants: Arc<[ShellCommandGrant]>,
}

#[derive(Debug, Default)]
pub(crate) struct SessionShellPermissions {
    revision: u64,
    next_id: u64,
    total_command_bytes: usize,
    grants: VecDeque<ShellCommandGrant>,
}

impl SessionShellPermissions {
    #[must_use]
    pub fn snapshot(&self) -> ShellPermissionSnapshot {
        ShellPermissionSnapshot {
            revision: self.revision,
            grants: self.grants.iter().cloned().collect::<Vec<_>>().into(),
        }
    }

    #[must_use]
    pub fn authorizes(&self, command: &str, decision: ConfirmationDecision) -> bool {
        session_grant_is_eligible(decision)
            && self.grants.iter().any(|grant| {
                grant.command_digest.matches_command(command) && grant.command == command
            })
    }

    pub fn grant_exact(
        &mut self,
        command: &str,
        command_digest: CommandDigest,
        turn_id: u64,
        action_id: u64,
    ) -> u64 {
        if let Some(existing) = self
            .grants
            .iter()
            .find(|grant| grant.command_digest == command_digest && grant.command == command)
        {
            return existing.id;
        }

        while self.grants.len() >= MAX_SESSION_SHELL_GRANTS
            || self.total_command_bytes.saturating_add(command.len())
                > MAX_SESSION_SHELL_GRANT_BYTES
        {
            let Some(expired) = self.grants.pop_front() else {
                break;
            };
            self.total_command_bytes = self
                .total_command_bytes
                .saturating_sub(expired.command.len());
        }

        self.next_id = self.next_id.saturating_add(1).max(1);
        let id = self.next_id;
        self.grants.push_back(ShellCommandGrant {
            id,
            command: command.to_owned(),
            command_digest,
            granted_turn_id: turn_id,
            granted_action_id: action_id,
        });
        self.total_command_bytes = self.total_command_bytes.saturating_add(command.len());
        self.revision = self.revision.saturating_add(1);
        id
    }

    pub fn revoke(&mut self, id: u64) -> bool {
        let Some(index) = self.grants.iter().position(|grant| grant.id == id) else {
            return false;
        };
        let Some(grant) = self.grants.remove(index) else {
            return false;
        };
        self.total_command_bytes = self.total_command_bytes.saturating_sub(grant.command.len());
        self.revision = self.revision.saturating_add(1);
        true
    }

    pub fn clear(&mut self) -> bool {
        if self.grants.is_empty() {
            return false;
        }
        self.grants.clear();
        self.total_command_bytes = 0;
        self.revision = self.revision.saturating_add(1);
        true
    }
}

#[must_use]
pub const fn session_grant_is_eligible(decision: ConfirmationDecision) -> bool {
    matches!(
        decision,
        ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::PolicyRequired | ConfirmationReason::NotAllowlisted
        }
    )
}

#[cfg(test)]
mod tests {
    use crate::tools::{CommandDigest, ConfirmationDecision, ConfirmationReason};

    use super::{MAX_SESSION_SHELL_GRANTS, SessionShellPermissions, session_grant_is_eligible};

    #[test]
    fn exact_grant_never_matches_a_nearby_command() {
        let mut permissions = SessionShellPermissions::default();
        let command = "cargo test --all-targets";
        permissions.grant_exact(command, CommandDigest::for_command(command), 7, 9);
        let local_policy = ConfirmationDecision::RequiresUserConfirmation {
            reason: ConfirmationReason::PolicyRequired,
        };

        assert!(permissions.authorizes(command, local_policy));
        assert!(!permissions.authorizes("cargo test --all-target", local_policy));
        assert!(!permissions.authorizes("cargo test --all-targets ", local_policy));
    }

    #[test]
    fn grant_cannot_override_model_or_forced_confirmation() {
        let mut permissions = SessionShellPermissions::default();
        let command = "cargo check";
        permissions.grant_exact(command, CommandDigest::for_command(command), 1, 1);

        for reason in [
            ConfirmationReason::ModelRequested,
            ConfirmationReason::ForcedRule("test forced rule"),
        ] {
            let decision = ConfirmationDecision::RequiresUserConfirmation { reason };
            assert!(!session_grant_is_eligible(decision));
            assert!(!permissions.authorizes(command, decision));
        }
    }

    #[test]
    fn grants_are_bounded_and_oldest_entry_is_evicted() {
        let mut permissions = SessionShellPermissions::default();
        for index in 0..=MAX_SESSION_SHELL_GRANTS {
            let command = format!("safe-read-{index}");
            permissions.grant_exact(
                &command,
                CommandDigest::for_command(&command),
                1,
                index as u64,
            );
        }
        let snapshot = permissions.snapshot();
        assert_eq!(snapshot.grants.len(), MAX_SESSION_SHELL_GRANTS);
        assert_eq!(snapshot.grants[0].command, "safe-read-1");
    }

    #[test]
    fn revoke_and_clear_are_revisioned_and_idempotent() {
        let mut permissions = SessionShellPermissions::default();
        let command = "git status";
        let id = permissions.grant_exact(command, CommandDigest::for_command(command), 1, 2);
        assert!(permissions.revoke(id));
        assert!(!permissions.revoke(id));
        let revision = permissions.snapshot().revision;
        assert!(!permissions.clear());
        assert_eq!(permissions.snapshot().revision, revision);
    }
}
