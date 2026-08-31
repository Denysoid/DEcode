use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_width::UnicodeWidthStr as _;

use crate::notice::UiNotice;

use super::state::TurnId;

pub const MAX_FOLLOW_UP_BYTES: usize = 16 * 1024;
pub const MAX_FOLLOW_UP_ITEMS: usize = 64;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FollowUpError {
    #[error("follow-up must contain visible text")]
    Empty,
    #[error("follow-up exceeds {MAX_FOLLOW_UP_BYTES} bytes")]
    TooLarge,
    #[error("follow-up queue already contains {MAX_FOLLOW_UP_ITEMS} live items")]
    QueueFull,
    #[error("follow-up {0} does not exist")]
    NotFound(u64),
    #[error("follow-up {id} changed (expected revision {expected}, current {actual})")]
    StaleRevision { id: u64, expected: u64, actual: u64 },
    #[error("follow-up {0} can no longer be changed")]
    NotMutable(u64),
    #[error("steer follow-up requires an active target turn")]
    MissingTargetTurn,
    #[error("follow-up contains unsupported control characters")]
    InvalidControl,
    #[error("follow-up identifier space is exhausted")]
    IdentifierExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpMode {
    Queue,
    Steer,
}

impl std::fmt::Display for FollowUpMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Queue => "Queue",
            Self::Steer => "Steer",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpStatus {
    Pending,
    Dispatching,
    Delivered,
    Cancelled,
    Failed,
}

impl FollowUpStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Delivered | Self::Cancelled | Self::Failed)
    }

    #[must_use]
    pub const fn is_mutable(self) -> bool {
        matches!(self, Self::Pending)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FollowUpItem {
    pub id: u64,
    pub revision: u64,
    pub mode: FollowUpMode,
    pub text: String,
    pub status: FollowUpStatus,
    #[serde(default, alias = "status_message")]
    pub notice: UiNotice,
    pub target_turn_id: Option<TurnId>,
    pub requires_manual_dispatch: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Default for FollowUpItem {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            id: 0,
            revision: 0,
            mode: FollowUpMode::Queue,
            text: String::new(),
            status: FollowUpStatus::Cancelled,
            notice: UiNotice::None,
            target_turn_id: None,
            requires_manual_dispatch: false,
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FollowUpState {
    revision: u64,
    next_id: u64,
    items: Vec<FollowUpItem>,
}

impl FollowUpState {
    pub fn enqueue(
        &mut self,
        mode: FollowUpMode,
        text: String,
        active_turn_id: Option<TurnId>,
    ) -> Result<FollowUpItem, FollowUpError> {
        validate_follow_up(&text)?;
        if mode == FollowUpMode::Steer && active_turn_id.is_none() {
            return Err(FollowUpError::MissingTargetTurn);
        }
        let id = self.next_id.max(1);
        let next_id = id
            .checked_add(1)
            .ok_or(FollowUpError::IdentifierExhausted)?;
        self.prune_terminal_for_capacity();
        if self.items.len() >= MAX_FOLLOW_UP_ITEMS {
            return Err(FollowUpError::QueueFull);
        }
        let now = Utc::now();
        let item = FollowUpItem {
            id,
            revision: 1,
            mode,
            text,
            status: FollowUpStatus::Pending,
            notice: match mode {
                FollowUpMode::Queue => UiNotice::FollowUpWaitingTurn,
                FollowUpMode::Steer => UiNotice::FollowUpWaitingBoundary,
            },
            target_turn_id: active_turn_id,
            requires_manual_dispatch: false,
            created_at: now,
            updated_at: now,
        };
        self.next_id = next_id;
        self.items.push(item.clone());
        self.bump_revision();
        Ok(item)
    }

    /// Queue work recovered or promoted from another UI surface without
    /// immediately dispatching it. This is used when the current work modes
    /// may still be read-only; the user must explicitly click Run next after
    /// reviewing modes and the generated prompt.
    pub fn enqueue_manual_queue(
        &mut self,
        text: String,
        notice: UiNotice,
    ) -> Result<FollowUpItem, FollowUpError> {
        let item = self.enqueue(FollowUpMode::Queue, text, None)?;
        let stored = self
            .items
            .iter_mut()
            .find(|candidate| candidate.id == item.id)
            .ok_or(FollowUpError::NotFound(item.id))?;
        stored.requires_manual_dispatch = true;
        stored.notice = notice;
        touch(stored);
        let item = stored.clone();
        self.bump_revision();
        Ok(item)
    }

    pub fn edit(
        &mut self,
        id: u64,
        expected_revision: u64,
        text: String,
    ) -> Result<(), FollowUpError> {
        validate_follow_up(&text)?;
        let item = self.checked_mut(id, expected_revision)?;
        if !matches!(
            item.status,
            FollowUpStatus::Pending | FollowUpStatus::Failed
        ) {
            return Err(FollowUpError::NotMutable(id));
        }
        item.text = text;
        item.notice = if item.status == FollowUpStatus::Failed {
            UiNotice::FollowUpEditedAfterFailure
        } else {
            UiNotice::FollowUpEditedPending
        };
        touch(item);
        self.bump_revision();
        Ok(())
    }

    pub fn cancel(&mut self, id: u64, expected_revision: u64) -> Result<(), FollowUpError> {
        let item = self.checked_mut(id, expected_revision)?;
        if !item.status.is_mutable() {
            return Err(FollowUpError::NotMutable(id));
        }
        item.status = FollowUpStatus::Cancelled;
        item.notice = UiNotice::FollowUpCancelledBeforeDelivery;
        touch(item);
        self.bump_revision();
        Ok(())
    }

    pub fn retry(
        &mut self,
        id: u64,
        expected_revision: u64,
        active_turn_id: Option<TurnId>,
    ) -> Result<(), FollowUpError> {
        let item = self.checked_mut(id, expected_revision)?;
        if item.status != FollowUpStatus::Failed {
            return Err(FollowUpError::NotMutable(id));
        }
        if item.mode == FollowUpMode::Steer && active_turn_id.is_none() {
            return Err(FollowUpError::MissingTargetTurn);
        }
        item.status = FollowUpStatus::Pending;
        item.target_turn_id = active_turn_id;
        item.requires_manual_dispatch = item.mode == FollowUpMode::Queue;
        item.notice = match item.mode {
            FollowUpMode::Queue => UiNotice::FollowUpRetryQueued,
            FollowUpMode::Steer => UiNotice::FollowUpRetrySteer,
        };
        touch(item);
        self.bump_revision();
        Ok(())
    }

    pub fn begin_dispatch(&mut self, id: u64, turn_id: TurnId) -> Result<(), FollowUpError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(FollowUpError::NotFound(id))?;
        if item.status != FollowUpStatus::Pending || item.mode != FollowUpMode::Queue {
            return Err(FollowUpError::NotMutable(id));
        }
        item.status = FollowUpStatus::Dispatching;
        item.target_turn_id = Some(turn_id);
        item.requires_manual_dispatch = false;
        item.notice = UiNotice::FollowUpDispatched;
        touch(item);
        self.bump_revision();
        Ok(())
    }

    pub fn deliver_steer(
        &mut self,
        id: u64,
        turn_id: TurnId,
    ) -> Result<FollowUpItem, FollowUpError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(FollowUpError::NotFound(id))?;
        if item.status != FollowUpStatus::Pending
            || item.mode != FollowUpMode::Steer
            || item.target_turn_id != Some(turn_id)
        {
            return Err(FollowUpError::NotMutable(id));
        }
        item.status = FollowUpStatus::Delivered;
        item.notice = UiNotice::FollowUpDeliveredInsideTurn { turn_id };
        touch(item);
        let delivered = item.clone();
        self.bump_revision();
        Ok(delivered)
    }

    pub fn mark_delivered(&mut self, id: u64) -> Result<(), FollowUpError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(FollowUpError::NotFound(id))?;
        if item.status != FollowUpStatus::Dispatching {
            return Err(FollowUpError::NotMutable(id));
        }
        item.status = FollowUpStatus::Delivered;
        item.notice = UiNotice::FollowUpDeliveredAsTurn {
            turn_id: item.target_turn_id,
        };
        touch(item);
        self.bump_revision();
        Ok(())
    }

    pub fn mark_failed(&mut self, id: u64, notice: UiNotice) -> Result<(), FollowUpError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(FollowUpError::NotFound(id))?;
        if item.status.is_terminal() {
            return Err(FollowUpError::NotMutable(id));
        }
        item.status = FollowUpStatus::Failed;
        item.notice = notice;
        touch(item);
        self.bump_revision();
        Ok(())
    }

    #[must_use]
    pub fn next_queue(&self) -> Option<&FollowUpItem> {
        self.items
            .iter()
            .find(|item| item.mode == FollowUpMode::Queue && item.status == FollowUpStatus::Pending)
    }

    #[must_use]
    pub fn next_auto_queue(&self) -> Option<&FollowUpItem> {
        self.next_queue()
            .filter(|item| !item.requires_manual_dispatch)
    }

    #[must_use]
    pub fn dispatching_for_turn(&self, turn_id: TurnId) -> Option<&FollowUpItem> {
        self.items.iter().find(|item| {
            item.mode == FollowUpMode::Queue
                && item.status == FollowUpStatus::Dispatching
                && item.target_turn_id == Some(turn_id)
        })
    }

    #[must_use]
    pub fn next_steer(&self, turn_id: TurnId) -> Option<&FollowUpItem> {
        self.items.iter().find(|item| {
            item.mode == FollowUpMode::Steer
                && item.status == FollowUpStatus::Pending
                && item.target_turn_id == Some(turn_id)
        })
    }

    pub fn fail_pending_steers_for_turn(&mut self, turn_id: TurnId, notice: UiNotice) {
        let mut changed = false;
        for item in &mut self.items {
            if item.mode == FollowUpMode::Steer
                && item.status == FollowUpStatus::Pending
                && item.target_turn_id == Some(turn_id)
            {
                item.status = FollowUpStatus::Failed;
                item.notice = notice.clone();
                touch(item);
                changed = true;
            }
        }
        if changed {
            self.bump_revision();
        }
    }

    pub fn recover_after_restart(&mut self) {
        let mut changed = false;
        for item in &mut self.items {
            match (item.mode, item.status) {
                (FollowUpMode::Steer, FollowUpStatus::Pending)
                | (_, FollowUpStatus::Dispatching) => {
                    item.status = FollowUpStatus::Failed;
                    item.notice = UiNotice::FollowUpInterrupted;
                    touch(item);
                    changed = true;
                }
                (FollowUpMode::Queue, FollowUpStatus::Pending) => {
                    item.requires_manual_dispatch = true;
                    item.notice = UiNotice::FollowUpRecoveredPending;
                    touch(item);
                    changed = true;
                }
                (
                    _,
                    FollowUpStatus::Delivered | FollowUpStatus::Cancelled | FollowUpStatus::Failed,
                ) => {}
            }
        }
        let next_existing = self
            .items
            .iter()
            .map(|item| item.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.next_id = self.next_id.max(next_existing);
        if changed {
            self.bump_revision();
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.next_id = 0;
        self.bump_revision();
    }

    #[must_use]
    pub fn snapshot(&self) -> FollowUpSnapshot {
        FollowUpSnapshot {
            revision: self.revision,
            items: Arc::from(self.items.clone()),
        }
    }

    fn checked_mut(
        &mut self,
        id: u64,
        expected_revision: u64,
    ) -> Result<&mut FollowUpItem, FollowUpError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or(FollowUpError::NotFound(id))?;
        if item.revision != expected_revision {
            return Err(FollowUpError::StaleRevision {
                id,
                expected: expected_revision,
                actual: item.revision,
            });
        }
        Ok(item)
    }

    fn prune_terminal_for_capacity(&mut self) {
        while self.items.len() >= MAX_FOLLOW_UP_ITEMS {
            let Some(index) = self.items.iter().position(|item| item.status.is_terminal()) else {
                break;
            };
            self.items.remove(index);
        }
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FollowUpSnapshot {
    pub revision: u64,
    pub items: Arc<[FollowUpItem]>,
}

impl FollowUpSnapshot {
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.items
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    FollowUpStatus::Pending | FollowUpStatus::Dispatching
                )
            })
            .count()
    }
}

pub fn validate_follow_up(text: &str) -> Result<(), FollowUpError> {
    if text.len() > MAX_FOLLOW_UP_BYTES {
        return Err(FollowUpError::TooLarge);
    }
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(FollowUpError::InvalidControl);
    }
    if text.trim().is_empty() || text.width() == 0 {
        return Err(FollowUpError::Empty);
    }
    Ok(())
}

fn touch(item: &mut FollowUpItem) {
    item.revision = item.revision.saturating_add(1);
    item.updated_at = Utc::now();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_fifo_revision_bound_and_restart_does_not_replay() -> Result<(), FollowUpError> {
        let mut state = FollowUpState::default();
        let first = state.enqueue(FollowUpMode::Queue, "first".to_owned(), None)?;
        let second = state.enqueue(FollowUpMode::Queue, "second".to_owned(), None)?;
        assert_eq!(state.next_queue().map(|item| item.id), Some(first.id));
        assert!(matches!(
            state.edit(
                first.id,
                first.revision.saturating_add(1),
                "stale".to_owned()
            ),
            Err(FollowUpError::StaleRevision { .. })
        ));
        state.begin_dispatch(first.id, 9)?;
        state.recover_after_restart();
        assert_eq!(state.next_queue().map(|item| item.id), Some(second.id));
        assert!(state.next_auto_queue().is_none());
        assert_eq!(state.items[0].status, FollowUpStatus::Failed);
        assert_eq!(state.items[1].status, FollowUpStatus::Pending);
        Ok(())
    }

    #[test]
    fn steer_is_bound_to_one_active_turn_and_delivered_once() -> Result<(), FollowUpError> {
        let mut state = FollowUpState::default();
        assert_eq!(
            state.enqueue(FollowUpMode::Steer, "pivot".to_owned(), None),
            Err(FollowUpError::MissingTargetTurn)
        );
        let steer = state.enqueue(FollowUpMode::Steer, "pivot".to_owned(), Some(7))?;
        assert!(state.next_steer(8).is_none());
        assert_eq!(state.next_steer(7).map(|item| item.id), Some(steer.id));
        let delivered = state.deliver_steer(steer.id, 7)?;
        assert_eq!(delivered.status, FollowUpStatus::Delivered);
        assert!(state.next_steer(7).is_none());
        assert!(matches!(
            state.deliver_steer(steer.id, 7),
            Err(FollowUpError::NotMutable(_))
        ));
        Ok(())
    }

    #[test]
    fn manually_queued_item_never_auto_dispatches() -> Result<(), FollowUpError> {
        let mut state = FollowUpState::default();
        let item = state.enqueue_manual_queue(
            "fix reviewed issue".to_owned(),
            UiNotice::FollowUpRecoveredPending,
        )?;
        assert!(item.requires_manual_dispatch);
        assert!(state.next_queue().is_some());
        assert!(state.next_auto_queue().is_none());
        Ok(())
    }

    #[test]
    fn invisible_and_control_only_follow_ups_are_rejected() {
        assert_eq!(
            validate_follow_up("\u{200b}\u{2060}"),
            Err(FollowUpError::Empty)
        );
        assert!(validate_follow_up("visible\u{1b}[2J").is_err());
    }

    #[test]
    fn restart_rebuilds_a_stale_follow_up_counter() -> Result<(), FollowUpError> {
        let mut state = FollowUpState::default();
        let first = state.enqueue(FollowUpMode::Queue, "first".to_owned(), None)?;
        state.mark_failed(first.id, UiNotice::FollowUpInterrupted)?;
        state.next_id = 0;

        state.recover_after_restart();
        let second = state.enqueue(FollowUpMode::Queue, "second".to_owned(), None)?;

        assert!(second.id > first.id);
        Ok(())
    }

    #[test]
    fn exhausted_follow_up_ids_fail_closed() {
        let mut state = FollowUpState {
            next_id: u64::MAX,
            ..FollowUpState::default()
        };

        assert!(
            state
                .enqueue(FollowUpMode::Queue, "next".to_owned(), None)
                .is_err()
        );
        assert!(state.items.is_empty());
    }
}
