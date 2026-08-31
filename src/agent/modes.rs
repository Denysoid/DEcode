use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::api::{ReasoningEffort, ReasoningMode};

use super::{side_chat::has_visible_text, state::TurnId};

pub const MAX_GOAL_BYTES: usize = 32 * 1024;
pub const MAX_PLAN_BYTES: usize = 128 * 1024;
const MAX_PROGRESS_TEXT_BYTES: usize = 16 * 1024;
const MAX_GOAL_STEPS: usize = 64;
const MAX_GOAL_STEP_BYTES: usize = 2 * 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ModeError {
    #[error("goal must contain visible text and be at most {MAX_GOAL_BYTES} bytes")]
    InvalidGoal,
    #[error("plan must contain visible text and be at most {MAX_PLAN_BYTES} bytes")]
    InvalidPlan,
    #[error("goal progress field {field:?} exceeds {limit} bytes")]
    ProgressTooLarge { field: &'static str, limit: usize },
    #[error("goal progress accepts at most {MAX_GOAL_STEPS} completed and next steps")]
    TooManySteps,
    #[error("goal progress step exceeds {MAX_GOAL_STEP_BYTES} bytes or is blank")]
    InvalidStep,
    #[error("goal mode is not active")]
    GoalInactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Completed,
    Blocked,
}

impl std::fmt::Display for GoalStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Blocked => "blocked",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalState {
    pub objective: String,
    pub status: GoalStatus,
    pub revision: u64,
    pub summary: String,
    pub completed_steps: Vec<String>,
    pub next_steps: Vec<String>,
    pub verification: String,
    pub updated_at: DateTime<Utc>,
    pub last_checked_turn: Option<TurnId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct WorkModes {
    pub plan: bool,
    pub explore: bool,
    pub review: bool,
    pub deep_thinking: bool,
    pub goal: Option<GoalState>,
}

impl WorkModes {
    #[must_use]
    pub const fn goal_enabled(&self) -> bool {
        self.goal.is_some()
    }

    #[must_use]
    pub const fn any_enabled(&self) -> bool {
        self.plan || self.explore || self.review || self.deep_thinking || self.goal.is_some()
    }

    /// Returns whether the harness must reject every operation that could
    /// change the workspace or invoke an external integration.
    #[must_use]
    pub const fn read_only(&self) -> bool {
        self.explore || self.review
    }

    #[must_use]
    pub fn effective_reasoning(
        &self,
        configured: ReasoningEffort,
    ) -> (ReasoningEffort, Option<ReasoningMode>) {
        let mut effort = configured;
        if self.plan {
            effort = effort.at_least(ReasoningEffort::XHigh);
        }
        if self.explore {
            effort = effort.at_least(ReasoningEffort::XHigh);
        }
        if self.review {
            effort = effort.at_least(ReasoningEffort::XHigh);
        }
        if self.goal.is_some() {
            effort = effort.at_least(ReasoningEffort::XHigh);
        }
        if self.deep_thinking {
            effort = effort.at_least(ReasoningEffort::Max);
        }
        (effort, self.deep_thinking.then_some(ReasoningMode::Pro))
    }

    #[must_use]
    pub fn active_summary(&self) -> String {
        let mut active = Vec::with_capacity(5);
        if self.plan {
            active.push("Plan");
        }
        if self.explore {
            active.push("Explore");
        }
        if self.review {
            active.push("Review");
        }
        if self.goal.is_some() {
            active.push("Goal");
        }
        if self.deep_thinking {
            active.push("Deep");
        }
        if active.is_empty() {
            "None".to_owned()
        } else {
            active.join(" + ")
        }
    }

    pub fn set_goal(&mut self, objective: Option<String>) -> Result<(), ModeError> {
        let Some(objective) = objective else {
            self.goal = None;
            return Ok(());
        };
        let objective =
            validate_visible_text(objective, MAX_GOAL_BYTES).map_err(|_| ModeError::InvalidGoal)?;
        let revision = self
            .goal
            .as_ref()
            .map_or(1, |goal| goal.revision.saturating_add(1));
        self.goal = Some(GoalState {
            objective,
            status: GoalStatus::Active,
            revision,
            summary: "Goal created; decomposition is pending.".to_owned(),
            completed_steps: Vec::new(),
            next_steps: Vec::new(),
            verification: String::new(),
            updated_at: Utc::now(),
            last_checked_turn: None,
        });
        Ok(())
    }

    pub fn update_goal(
        &mut self,
        turn_id: TurnId,
        update: GoalUpdate,
    ) -> Result<&GoalState, ModeError> {
        let goal = self.goal.as_mut().ok_or(ModeError::GoalInactive)?;
        validate_progress_text("summary", &update.summary)?;
        validate_progress_text("verification", &update.verification)?;
        if update.completed_steps.len() > MAX_GOAL_STEPS || update.next_steps.len() > MAX_GOAL_STEPS
        {
            return Err(ModeError::TooManySteps);
        }
        validate_steps(&update.completed_steps)?;
        validate_steps(&update.next_steps)?;
        goal.status = update.status;
        goal.summary = update.summary;
        goal.completed_steps = update.completed_steps;
        goal.next_steps = update.next_steps;
        goal.verification = update.verification;
        goal.revision = goal.revision.saturating_add(1);
        goal.updated_at = Utc::now();
        goal.last_checked_turn = Some(turn_id);
        Ok(goal)
    }

    #[must_use]
    pub fn instruction_suffix(&self) -> String {
        let mut instructions = String::new();
        if self.plan {
            instructions.push_str(
                "\n\nPLAN MODE IS ACTIVE. Before implementation, the harness runs a separate read-only planning pass and requires explicit user approval. During implementation, follow the approved plan and call out material deviations before acting.",
            );
        }
        if self.explore {
            instructions.push_str(
                "\n\nEXPLORE MODE IS ACTIVE. The harness is strictly read-only: inspect and explain the repository using read_file, list_directory, search_code, repository intelligence, and LSP tools. Do not request file writes, patches, shell commands, MCP calls, or sub-agent actions; the harness rejects them fail-closed even if they appear harmless. Give evidence with file paths and clearly separate observations from recommendations.",
            );
        }
        if self.review {
            instructions.push_str(
                "\n\nREVIEW MODE IS ACTIVE. The harness captured one immutable Git diff snapshot before this turn and blocks all workspace mutations, shell commands, MCP calls, and sub-agent actions. Inspect the complete snapshot through the native review_diff tool, paging until complete=true. Use repository intelligence, LSP, and read-only file tools only to validate concrete defects introduced by that snapshot. Do not report speculative style preferences. Before finishing, call submit_review exactly once with the snapshot SHA-256, a concise verdict, and actionable findings with precise file/line evidence. A pass must use an empty findings array. Never treat current mutable file contents as proof that the captured diff changed after review began.",
            );
        }
        if let Some(goal) = &self.goal {
            let objective = serde_json::to_string(&goal.objective)
                .unwrap_or_else(|_| "\"[goal unavailable]\"".to_owned());
            instructions.push_str(&format!(
                "\n\nGOAL MODE IS ACTIVE. Persistent top-level objective: {objective}. Decompose it into bounded steps, compare progress against the top-level objective after tool rounds, and re-plan when evidence contradicts the current approach. Before treating the current user turn as complete, call the native update_goal tool with an honest verification summary. Do not expand authority beyond the objective. Current goal status: {}; current progress summary: {}",
                goal.status, goal.summary
            ));
        }
        if self.deep_thinking {
            instructions.push_str(
                "\n\nDEEP THINKING MODE IS ACTIVE. Use maximum supported reasoning. Before the final answer, test the conclusion against plausible alternatives, inspect failure modes, and perform an explicit verification pass. Present only a concise alternatives/tradeoffs summary and verification evidence; do not expose private chain-of-thought.",
            );
        }
        instructions
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUpdate {
    pub status: GoalStatus,
    pub summary: String,
    pub completed_steps: Vec<String>,
    pub next_steps: Vec<String>,
    pub verification: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanReview {
    pub turn_id: TurnId,
    pub review_id: u64,
    pub plan: String,
    pub deployment: String,
    pub reasoning_effort: ReasoningEffort,
    pub reasoning_mode: Option<ReasoningMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    Approve { plan: String },
    Reject,
}

pub fn validate_plan(plan: String) -> Result<String, ModeError> {
    validate_visible_text(plan, MAX_PLAN_BYTES).map_err(|_| ModeError::InvalidPlan)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InvalidVisibleText;

fn validate_visible_text(value: String, max_bytes: usize) -> Result<String, InvalidVisibleText> {
    if !has_visible_text(&value) || value.len() > max_bytes || value.contains('\0') {
        return Err(InvalidVisibleText);
    }
    Ok(value)
}

fn validate_progress_text(field: &'static str, value: &str) -> Result<(), ModeError> {
    if value.len() > MAX_PROGRESS_TEXT_BYTES || value.contains('\0') {
        return Err(ModeError::ProgressTooLarge {
            field,
            limit: MAX_PROGRESS_TEXT_BYTES,
        });
    }
    Ok(())
}

fn validate_steps(steps: &[String]) -> Result<(), ModeError> {
    if steps.iter().any(|step| {
        !has_visible_text(step) || step.len() > MAX_GOAL_STEP_BYTES || step.contains('\0')
    }) {
        return Err(ModeError::InvalidStep);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_active_mode_raises_effort_and_deep_uses_pro() -> Result<(), ModeError> {
        let mut modes = WorkModes {
            plan: true,
            ..WorkModes::default()
        };
        assert_eq!(
            modes.effective_reasoning(ReasoningEffort::Low),
            (ReasoningEffort::XHigh, None)
        );
        modes.plan = false;
        modes.set_goal(Some("Ship a verified fix".to_owned()))?;
        assert_eq!(
            modes.effective_reasoning(ReasoningEffort::High),
            (ReasoningEffort::XHigh, None)
        );
        modes.deep_thinking = true;
        assert_eq!(
            modes.effective_reasoning(ReasoningEffort::Low),
            (ReasoningEffort::Max, Some(ReasoningMode::Pro))
        );
        Ok(())
    }

    #[test]
    fn all_thirty_two_mode_combinations_are_independent_and_take_maximum_effort()
    -> Result<(), ModeError> {
        for plan in [false, true] {
            for explore in [false, true] {
                for review in [false, true] {
                    for goal in [false, true] {
                        for deep_thinking in [false, true] {
                            let mut modes = WorkModes {
                                plan,
                                explore,
                                review,
                                deep_thinking,
                                goal: None,
                            };
                            if goal {
                                modes.set_goal(Some("Keep every flag independent".to_owned()))?;
                            }
                            let expected = if deep_thinking {
                                (ReasoningEffort::Max, Some(ReasoningMode::Pro))
                            } else if plan || explore || review || goal {
                                (ReasoningEffort::XHigh, None)
                            } else {
                                (ReasoningEffort::Low, None)
                            };
                            assert_eq!(modes.effective_reasoning(ReasoningEffort::Low), expected);
                            assert_eq!(modes.plan, plan);
                            assert_eq!(modes.explore, explore);
                            assert_eq!(modes.review, review);
                            assert_eq!(modes.goal_enabled(), goal);
                            assert_eq!(modes.deep_thinking, deep_thinking);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn active_summary_names_simultaneous_modes() -> Result<(), ModeError> {
        let mut modes = WorkModes {
            plan: true,
            explore: true,
            review: true,
            deep_thinking: true,
            goal: None,
        };
        modes.set_goal(Some("Ship safely".to_owned()))?;
        assert_eq!(
            modes.active_summary(),
            "Plan + Explore + Review + Goal + Deep"
        );
        Ok(())
    }

    #[test]
    fn goal_progress_is_bounded_and_records_turn() -> Result<(), ModeError> {
        let mut modes = WorkModes::default();
        modes.set_goal(Some("Finish the parser".to_owned()))?;
        let goal = modes.update_goal(
            9,
            GoalUpdate {
                status: GoalStatus::Active,
                summary: "Parser fixed; integration tests remain.".to_owned(),
                completed_steps: vec!["Fix scanner".to_owned()],
                next_steps: vec!["Run integration tests".to_owned()],
                verification: "Unit tests passed.".to_owned(),
            },
        )?;
        assert_eq!(goal.last_checked_turn, Some(9));
        assert_eq!(goal.revision, 2);
        Ok(())
    }

    #[test]
    fn modes_and_goal_survive_agent_state_json_round_trip() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut state = crate::agent::state::AgentState::new();
        state.work_modes.plan = true;
        state.work_modes.explore = true;
        state.work_modes.review = true;
        state.work_modes.deep_thinking = true;
        state.record_deployment_usage("coding-prod", 12, 2, 4, 16, 1);
        state
            .work_modes
            .set_goal(Some("Preserve this objective across resume".to_owned()))?;
        let encoded = serde_json::to_string(&state)?;
        let decoded: crate::agent::state::AgentState = serde_json::from_str(&encoded)?;
        assert!(decoded.work_modes.plan);
        assert!(decoded.work_modes.explore);
        assert!(decoded.work_modes.review);
        assert!(decoded.work_modes.deep_thinking);
        assert_eq!(
            decoded
                .work_modes
                .goal
                .as_ref()
                .map(|goal| goal.objective.as_str()),
            Some("Preserve this objective across resume")
        );
        let usage = crate::usage::PricingCatalog::default()
            .snapshot(&decoded.billing_usage, decoded.last_reported_total_tokens);
        assert_eq!(usage.usage.total_tokens, 16);
        Ok(())
    }

    #[test]
    fn sessions_written_before_new_optional_fields_remain_loadable()
    -> Result<(), Box<dyn std::error::Error>> {
        let state = crate::agent::state::AgentState::new();
        let mut encoded = serde_json::to_value(state)?;
        if let Some(work_modes) = encoded
            .get_mut("work_modes")
            .and_then(serde_json::Value::as_object_mut)
        {
            work_modes.remove("explore");
            work_modes.remove("review");
        }
        if let Some(state) = encoded.as_object_mut() {
            state.remove("billing_usage");
            state.remove("side_chat");
            state.remove("follow_ups");
            state.remove("reviews");
        }
        let decoded: crate::agent::state::AgentState = serde_json::from_value(encoded)?;
        assert!(!decoded.work_modes.explore);
        assert!(!decoded.work_modes.review);
        assert!(decoded.billing_usage.is_empty());
        assert!(decoded.side_chat.is_empty());
        assert!(decoded.follow_ups.snapshot().items.is_empty());
        assert!(decoded.reviews.snapshot().reports.is_empty());
        Ok(())
    }

    #[test]
    fn goals_and_plans_require_renderable_text() {
        let invisible = "\u{200b}\u{200d}".to_owned();
        let mut modes = WorkModes::default();

        assert_eq!(
            modes.set_goal(Some(invisible.clone())),
            Err(ModeError::InvalidGoal)
        );
        assert_eq!(validate_plan(invisible), Err(ModeError::InvalidPlan));
    }

    #[test]
    fn goal_steps_require_renderable_text() -> Result<(), ModeError> {
        let mut modes = WorkModes::default();
        modes.set_goal(Some("Finish the audit".to_owned()))?;

        let result = modes.update_goal(
            1,
            GoalUpdate {
                status: GoalStatus::Active,
                summary: String::new(),
                completed_steps: vec!["\u{200b}\u{200d}".to_owned()],
                next_steps: Vec::new(),
                verification: String::new(),
            },
        );

        assert_eq!(result, Err(ModeError::InvalidStep));
        Ok(())
    }
}
