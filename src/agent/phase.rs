use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPhase {
    Idle,
    PreparingReview,
    Planning,
    AwaitingPlanApproval,
    Requesting,
    Streaming,
    Parsing,
    AwaitingPatchApproval,
    AwaitingConfirmation,
    ExecutingTools,
    AwaitingContinuation,
    Error { message: String, recoverable: bool },
}

impl AgentPhase {
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::PreparingReview
                | Self::Planning
                | Self::AwaitingPlanApproval
                | Self::Requesting
                | Self::Streaming
                | Self::Parsing
                | Self::AwaitingPatchApproval
                | Self::AwaitingConfirmation
                | Self::ExecutingTools
                | Self::AwaitingContinuation
        )
    }

    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    #[must_use]
    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Error { message, .. } => Some(message.as_str()),
            _ => None,
        }
    }
}

impl std::fmt::Display for AgentPhase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Idle => "idle",
            Self::PreparingReview => "capturing immutable review diff",
            Self::Planning => "planning (read-only)",
            Self::AwaitingPlanApproval => "awaiting plan approval",
            Self::Requesting => "requesting",
            Self::Streaming => "streaming",
            Self::Parsing => "parsing",
            Self::AwaitingPatchApproval => "awaiting patch approval",
            Self::AwaitingConfirmation => "awaiting confirmation",
            Self::ExecutingTools => "executing tools",
            Self::AwaitingContinuation => "awaiting continuation",
            Self::Error { message, .. } => return write!(formatter, "error: {message}"),
        };
        formatter.write_str(label)
    }
}
