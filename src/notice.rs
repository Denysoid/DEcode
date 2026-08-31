use serde::{Deserialize, Deserializer, Serialize};

/// Language-neutral status emitted by backend services and localized only at
/// the presentation boundary. `Legacy` preserves old session journals without
/// treating their text as a new application-owned message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiNotice {
    #[default]
    None,
    FollowUpWaitingTurn,
    FollowUpWaitingBoundary,
    FollowUpEditedAfterFailure,
    FollowUpEditedPending,
    FollowUpCancelledBeforeDelivery,
    FollowUpRetryQueued,
    FollowUpRetrySteer,
    FollowUpDispatched,
    FollowUpDeliveredInsideTurn {
        turn_id: u64,
    },
    FollowUpDeliveredAsTurn {
        turn_id: Option<u64>,
    },
    FollowUpInterrupted,
    FollowUpRecoveredPending,
    SideQuestionRunning,
    SideAnswerProvisional,
    SideQuestionCancelled,
    SideQuestionInterrupted,
    SideToolCallBlocked,
    SideAnswerEmpty,
    StaleUiAction,
    TrustedDeploymentRequired,
    PersistenceBlocked,
    DependencyFailure,
    SteerRequiresActiveTurn,
    LexicalFallback {
        detail: String,
    },
    McpToolsReady {
        count: usize,
    },
    SubagentMcpDisabled,
    SubagentMcpStarting,
    CodeIndexScanning {
        count: usize,
    },
    CodeIndexDisabled,
    CodeIndexEmpty,
    CodeIndexLoading,
    CodeIndexRefreshing,
    CodeIndexRebuilding,
    CodeIndexCancelling,
    CodeIndexCancelled,
    CodeIndexReady,
    CodeIndexProgress {
        scanned: usize,
        reused: usize,
        changed: usize,
        skipped: usize,
    },
    EmbeddingDisabled,
    EmbeddingConfigured,
    EmbeddingCancelling,
    EmbeddingCacheMissing,
    EmbeddingNoChunks,
    EmbeddingReady {
        count: usize,
        reused: usize,
        embedded: usize,
    },
    EmbeddingPrivacyRefresh,
    LspStarting,
    LspReady,
    Stopped,
    ExternalError {
        detail: String,
    },
    Legacy {
        detail: String,
    },
}

impl UiNotice {
    #[must_use]
    pub const fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    #[must_use]
    pub fn external(detail: impl Into<String>) -> Self {
        Self::ExternalError {
            detail: detail.into(),
        }
    }
}

impl<'de> Deserialize<'de> for UiNotice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum Structured {
            None,
            FollowUpWaitingTurn,
            FollowUpWaitingBoundary,
            FollowUpEditedAfterFailure,
            FollowUpEditedPending,
            FollowUpCancelledBeforeDelivery,
            FollowUpRetryQueued,
            FollowUpRetrySteer,
            FollowUpDispatched,
            FollowUpDeliveredInsideTurn {
                turn_id: u64,
            },
            FollowUpDeliveredAsTurn {
                turn_id: Option<u64>,
            },
            FollowUpInterrupted,
            FollowUpRecoveredPending,
            SideQuestionRunning,
            SideAnswerProvisional,
            SideQuestionCancelled,
            SideQuestionInterrupted,
            SideToolCallBlocked,
            SideAnswerEmpty,
            StaleUiAction,
            TrustedDeploymentRequired,
            PersistenceBlocked,
            DependencyFailure,
            SteerRequiresActiveTurn,
            LexicalFallback {
                detail: String,
            },
            McpToolsReady {
                count: usize,
            },
            SubagentMcpDisabled,
            SubagentMcpStarting,
            CodeIndexScanning {
                count: usize,
            },
            CodeIndexDisabled,
            CodeIndexEmpty,
            CodeIndexLoading,
            CodeIndexRefreshing,
            CodeIndexRebuilding,
            CodeIndexCancelling,
            CodeIndexCancelled,
            CodeIndexReady,
            CodeIndexProgress {
                scanned: usize,
                reused: usize,
                changed: usize,
                skipped: usize,
            },
            EmbeddingDisabled,
            EmbeddingConfigured,
            EmbeddingCancelling,
            EmbeddingCacheMissing,
            EmbeddingNoChunks,
            EmbeddingReady {
                count: usize,
                reused: usize,
                embedded: usize,
            },
            EmbeddingPrivacyRefresh,
            LspStarting,
            LspReady,
            Stopped,
            ExternalError {
                detail: String,
            },
            Legacy {
                detail: String,
            },
        }

        impl From<Structured> for UiNotice {
            fn from(value: Structured) -> Self {
                match value {
                    Structured::None => Self::None,
                    Structured::FollowUpWaitingTurn => Self::FollowUpWaitingTurn,
                    Structured::FollowUpWaitingBoundary => Self::FollowUpWaitingBoundary,
                    Structured::FollowUpEditedAfterFailure => Self::FollowUpEditedAfterFailure,
                    Structured::FollowUpEditedPending => Self::FollowUpEditedPending,
                    Structured::FollowUpCancelledBeforeDelivery => {
                        Self::FollowUpCancelledBeforeDelivery
                    }
                    Structured::FollowUpRetryQueued => Self::FollowUpRetryQueued,
                    Structured::FollowUpRetrySteer => Self::FollowUpRetrySteer,
                    Structured::FollowUpDispatched => Self::FollowUpDispatched,
                    Structured::FollowUpDeliveredInsideTurn { turn_id } => {
                        Self::FollowUpDeliveredInsideTurn { turn_id }
                    }
                    Structured::FollowUpDeliveredAsTurn { turn_id } => {
                        Self::FollowUpDeliveredAsTurn { turn_id }
                    }
                    Structured::FollowUpInterrupted => Self::FollowUpInterrupted,
                    Structured::FollowUpRecoveredPending => Self::FollowUpRecoveredPending,
                    Structured::SideQuestionRunning => Self::SideQuestionRunning,
                    Structured::SideAnswerProvisional => Self::SideAnswerProvisional,
                    Structured::SideQuestionCancelled => Self::SideQuestionCancelled,
                    Structured::SideQuestionInterrupted => Self::SideQuestionInterrupted,
                    Structured::SideToolCallBlocked => Self::SideToolCallBlocked,
                    Structured::SideAnswerEmpty => Self::SideAnswerEmpty,
                    Structured::StaleUiAction => Self::StaleUiAction,
                    Structured::TrustedDeploymentRequired => Self::TrustedDeploymentRequired,
                    Structured::PersistenceBlocked => Self::PersistenceBlocked,
                    Structured::DependencyFailure => Self::DependencyFailure,
                    Structured::SteerRequiresActiveTurn => Self::SteerRequiresActiveTurn,
                    Structured::LexicalFallback { detail } => Self::LexicalFallback { detail },
                    Structured::McpToolsReady { count } => Self::McpToolsReady { count },
                    Structured::SubagentMcpDisabled => Self::SubagentMcpDisabled,
                    Structured::SubagentMcpStarting => Self::SubagentMcpStarting,
                    Structured::CodeIndexScanning { count } => Self::CodeIndexScanning { count },
                    Structured::CodeIndexDisabled => Self::CodeIndexDisabled,
                    Structured::CodeIndexEmpty => Self::CodeIndexEmpty,
                    Structured::CodeIndexLoading => Self::CodeIndexLoading,
                    Structured::CodeIndexRefreshing => Self::CodeIndexRefreshing,
                    Structured::CodeIndexRebuilding => Self::CodeIndexRebuilding,
                    Structured::CodeIndexCancelling => Self::CodeIndexCancelling,
                    Structured::CodeIndexCancelled => Self::CodeIndexCancelled,
                    Structured::CodeIndexReady => Self::CodeIndexReady,
                    Structured::CodeIndexProgress {
                        scanned,
                        reused,
                        changed,
                        skipped,
                    } => Self::CodeIndexProgress {
                        scanned,
                        reused,
                        changed,
                        skipped,
                    },
                    Structured::EmbeddingDisabled => Self::EmbeddingDisabled,
                    Structured::EmbeddingConfigured => Self::EmbeddingConfigured,
                    Structured::EmbeddingCancelling => Self::EmbeddingCancelling,
                    Structured::EmbeddingCacheMissing => Self::EmbeddingCacheMissing,
                    Structured::EmbeddingNoChunks => Self::EmbeddingNoChunks,
                    Structured::EmbeddingReady {
                        count,
                        reused,
                        embedded,
                    } => Self::EmbeddingReady {
                        count,
                        reused,
                        embedded,
                    },
                    Structured::EmbeddingPrivacyRefresh => Self::EmbeddingPrivacyRefresh,
                    Structured::LspStarting => Self::LspStarting,
                    Structured::LspReady => Self::LspReady,
                    Structured::Stopped => Self::Stopped,
                    Structured::ExternalError { detail } => Self::ExternalError { detail },
                    Structured::Legacy { detail } => Self::Legacy { detail },
                }
            }
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Compatible {
            Structured(Structured),
            Legacy(String),
        }

        Ok(match Compatible::deserialize(deserializer)? {
            Compatible::Structured(value) => value.into(),
            Compatible::Legacy(detail) if detail.is_empty() => Self::None,
            Compatible::Legacy(detail) => Self::Legacy { detail },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_string_notices_remain_readable() -> Result<(), serde_json::Error> {
        let notice: UiNotice = serde_json::from_str("\"old persisted status\"")?;
        assert_eq!(
            notice,
            UiNotice::Legacy {
                detail: "old persisted status".to_owned()
            }
        );
        Ok(())
    }

    #[test]
    fn structured_notice_round_trips() -> Result<(), serde_json::Error> {
        let notice = UiNotice::FollowUpDeliveredInsideTurn { turn_id: 42 };
        let encoded = serde_json::to_string(&notice)?;
        assert_eq!(serde_json::from_str::<UiNotice>(&encoded)?, notice);
        Ok(())
    }
}
