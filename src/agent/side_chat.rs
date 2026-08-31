use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_width::UnicodeWidthStr as _;

use crate::{api::ReasoningEffort, notice::UiNotice};

pub const MAX_SIDE_QUESTION_BYTES: usize = 16 * 1024;
pub const MAX_SIDE_ANSWER_BYTES: usize = 128 * 1024;
const MAX_SIDE_EXCHANGES: usize = 32;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SideChatError {
    #[error("side question must contain visible text")]
    EmptyQuestion,
    #[error("side question exceeds {MAX_SIDE_QUESTION_BYTES} bytes")]
    QuestionTooLarge,
    #[error("another side question is already running")]
    AlreadyRunning,
    #[error("side question {0} is no longer active")]
    StaleRequest(u64),
    #[error("side question identifier space is exhausted")]
    IdentifierExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideExchangeStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl SideExchangeStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SideExchange {
    pub id: u64,
    pub context_revision: u64,
    pub question: String,
    pub answer: String,
    pub deployment: String,
    pub reasoning_effort: ReasoningEffort,
    pub status: SideExchangeStatus,
    #[serde(default, alias = "status_message")]
    pub notice: UiNotice,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl Default for SideExchange {
    fn default() -> Self {
        Self {
            id: 0,
            context_revision: 0,
            question: String::new(),
            answer: String::new(),
            deployment: String::new(),
            reasoning_effort: ReasoningEffort::Medium,
            status: SideExchangeStatus::Cancelled,
            notice: UiNotice::None,
            created_at: Utc::now(),
            completed_at: None,
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SideChatState {
    revision: u64,
    next_id: u64,
    exchanges: Vec<SideExchange>,
}

impl SideChatState {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exchanges.is_empty()
    }

    #[must_use]
    pub fn exchanges(&self) -> &[SideExchange] {
        &self.exchanges
    }

    pub fn start(
        &mut self,
        question: String,
        context_revision: u64,
        deployment: String,
        reasoning_effort: ReasoningEffort,
    ) -> Result<SideExchange, SideChatError> {
        validate_question(&question)?;
        if self
            .exchanges
            .last()
            .is_some_and(|exchange| exchange.status == SideExchangeStatus::Running)
        {
            return Err(SideChatError::AlreadyRunning);
        }
        let id = self.next_id.max(1);
        let next_id = id
            .checked_add(1)
            .ok_or(SideChatError::IdentifierExhausted)?;
        let exchange = SideExchange {
            id,
            context_revision,
            question,
            deployment,
            reasoning_effort,
            status: SideExchangeStatus::Running,
            notice: UiNotice::SideQuestionRunning,
            created_at: Utc::now(),
            ..SideExchange::default()
        };
        self.next_id = next_id;
        self.exchanges.push(exchange.clone());
        if self.exchanges.len() > MAX_SIDE_EXCHANGES {
            let remove = self.exchanges.len().saturating_sub(MAX_SIDE_EXCHANGES);
            self.exchanges.drain(..remove);
        }
        self.bump_revision();
        Ok(exchange)
    }

    pub fn complete(
        &mut self,
        id: u64,
        answer: String,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    ) -> Result<(), SideChatError> {
        let exchange = self.active_mut(id)?;
        exchange.answer = bound_side_answer(answer);
        exchange.status = SideExchangeStatus::Completed;
        exchange.notice = UiNotice::SideAnswerProvisional;
        exchange.completed_at = Some(Utc::now());
        exchange.input_tokens = input_tokens;
        exchange.cached_input_tokens = cached_input_tokens.min(input_tokens);
        exchange.output_tokens = output_tokens;
        exchange.total_tokens = total_tokens;
        self.bump_revision();
        Ok(())
    }

    pub fn fail(&mut self, id: u64, notice: UiNotice) -> Result<(), SideChatError> {
        let exchange = self.active_mut(id)?;
        exchange.status = SideExchangeStatus::Failed;
        exchange.notice = notice;
        exchange.completed_at = Some(Utc::now());
        self.bump_revision();
        Ok(())
    }

    pub fn cancel(&mut self, id: u64) -> Result<(), SideChatError> {
        let exchange = self.active_mut(id)?;
        exchange.status = SideExchangeStatus::Cancelled;
        exchange.notice = UiNotice::SideQuestionCancelled;
        exchange.completed_at = Some(Utc::now());
        self.bump_revision();
        Ok(())
    }

    pub fn recover_after_restart(&mut self) {
        let mut changed = false;
        for exchange in &mut self.exchanges {
            if exchange.status == SideExchangeStatus::Running {
                exchange.status = SideExchangeStatus::Cancelled;
                exchange.notice = UiNotice::SideQuestionInterrupted;
                exchange.completed_at = Some(Utc::now());
                changed = true;
            }
        }
        let next_existing = self
            .exchanges
            .iter()
            .map(|exchange| exchange.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.next_id = self.next_id.max(next_existing);
        if changed {
            self.bump_revision();
        }
    }

    pub fn clear(&mut self) {
        self.exchanges.clear();
        self.next_id = 0;
        self.bump_revision();
    }

    #[must_use]
    pub fn snapshot(&self) -> SideChatSnapshot {
        SideChatSnapshot {
            revision: self.revision,
            exchanges: Arc::from(self.exchanges.clone()),
        }
    }

    fn active_mut(&mut self, id: u64) -> Result<&mut SideExchange, SideChatError> {
        self.exchanges
            .last_mut()
            .filter(|exchange| exchange.id == id && exchange.status == SideExchangeStatus::Running)
            .ok_or(SideChatError::StaleRequest(id))
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideChatSnapshot {
    pub revision: u64,
    pub exchanges: Arc<[SideExchange]>,
}

impl SideChatSnapshot {
    #[must_use]
    pub fn latest(&self) -> Option<&SideExchange> {
        self.exchanges.last()
    }

    pub fn apply_preview(&mut self, updated: SideExchange) -> bool {
        let Some(index) = self
            .exchanges
            .iter()
            .position(|exchange| exchange.id == updated.id)
        else {
            return false;
        };
        let mut exchanges = self.exchanges.to_vec();
        exchanges[index] = updated;
        self.revision = self.revision.saturating_add(1);
        self.exchanges = Arc::from(exchanges);
        true
    }
}

pub fn validate_question(question: &str) -> Result<(), SideChatError> {
    if question.len() > MAX_SIDE_QUESTION_BYTES {
        return Err(SideChatError::QuestionTooLarge);
    }
    if !has_visible_text(question) {
        return Err(SideChatError::EmptyQuestion);
    }
    Ok(())
}

#[must_use]
pub(crate) fn has_visible_text(value: &str) -> bool {
    !value.trim().is_empty()
        && value.width() > 0
        && value
            .chars()
            .any(|character| !character.is_control() && !character.is_whitespace())
}

#[must_use]
pub fn bound_side_answer(value: String) -> String {
    bounded(value, MAX_SIDE_ANSWER_BYTES)
}

fn bounded(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    const MARKER: &str = "\n[…side answer truncated…]";
    let mut end = limit.saturating_sub(MARKER.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value.push_str(MARKER);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_chat_is_bounded_separate_and_stale_completions_fail_closed() -> Result<(), SideChatError>
    {
        let mut state = SideChatState::default();
        let exchange = state.start(
            "Explain the retry boundary".to_owned(),
            17,
            "fast-model".to_owned(),
            ReasoningEffort::Medium,
        )?;
        assert!(matches!(
            state.start(
                "second".to_owned(),
                17,
                "fast-model".to_owned(),
                ReasoningEffort::Low,
            ),
            Err(SideChatError::AlreadyRunning)
        ));
        state.complete(exchange.id, "answer".to_owned(), 10, 99, 5, 15)?;
        let latest = state
            .snapshot()
            .latest()
            .cloned()
            .ok_or(SideChatError::StaleRequest(exchange.id))?;
        assert_eq!(latest.cached_input_tokens, 10);
        assert!(latest.status.is_terminal());
        assert!(matches!(
            state.complete(exchange.id, "late".to_owned(), 1, 0, 1, 2),
            Err(SideChatError::StaleRequest(_))
        ));
        Ok(())
    }

    #[test]
    fn interrupted_side_question_is_never_replayed_after_restart() -> Result<(), SideChatError> {
        let mut state = SideChatState::default();
        state.start(
            "What is running?".to_owned(),
            2,
            "model".to_owned(),
            ReasoningEffort::Medium,
        )?;
        state.recover_after_restart();
        assert_eq!(
            state.snapshot().latest().map(|exchange| exchange.status),
            Some(SideExchangeStatus::Cancelled)
        );
        Ok(())
    }

    #[test]
    fn invisible_question_is_rejected() {
        assert_eq!(
            validate_question("\u{200b}\u{2060}"),
            Err(SideChatError::EmptyQuestion)
        );
    }

    #[test]
    fn truncated_answer_including_marker_stays_within_the_limit() {
        let answer = bound_side_answer("ž".repeat(MAX_SIDE_ANSWER_BYTES));
        assert!(answer.len() <= MAX_SIDE_ANSWER_BYTES);
        assert!(answer.ends_with("[…side answer truncated…]"));
        assert!(answer.is_char_boundary(answer.len()));
    }

    #[test]
    fn exhausted_exchange_ids_fail_closed() {
        let mut state = SideChatState {
            next_id: u64::MAX,
            ..SideChatState::default()
        };
        assert!(
            state
                .start(
                    "question".to_owned(),
                    1,
                    "model".to_owned(),
                    ReasoningEffort::Medium,
                )
                .is_err()
        );
        assert!(state.exchanges.is_empty());
    }

    #[test]
    fn restart_rebuilds_a_stale_exchange_counter() -> Result<(), SideChatError> {
        let mut state = SideChatState::default();
        let first = state.start(
            "first".to_owned(),
            1,
            "model".to_owned(),
            ReasoningEffort::Medium,
        )?;
        state.complete(first.id, "done".to_owned(), 0, 0, 0, 0)?;
        state.next_id = 0;

        state.recover_after_restart();
        let second = state.start(
            "second".to_owned(),
            2,
            "model".to_owned(),
            ReasoningEffort::Medium,
        )?;

        assert!(second.id > first.id);
        Ok(())
    }
}
