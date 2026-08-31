use serde::{Deserialize, Serialize};

use super::tool_action::ToolAction;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ParserEvent {
    ThinkingDelta(String),
    ThinkingEnd,
    ToolCallParsed(ToolAction),
    ToolCallParseError { raw_tag: String, reason: String },
    TurnComplete { had_tool_calls: bool },
}
