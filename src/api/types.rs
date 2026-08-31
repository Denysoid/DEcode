use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::error::ApiError;

/// Roles accepted by a message item in the Responses API `input` array.
///
/// This is deliberately named `InputMessage`: it is an item inside the
/// top-level `input` field, not a Chat Completions `messages` request.
/// Legacy XML-tool results intentionally do not have a synthetic `tool` role.
/// They are encoded as structured JSON inside a user message by `tool_result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputMessage {
    pub role: Role,
    pub content: String,
}

impl InputMessage {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn developer(content: impl Into<String>) -> Self {
        Self {
            role: Role::Developer,
            content: content.into(),
        }
    }

    /// Serialize a canonical tool outcome envelope as a user message.
    pub fn tool_result(
        action_id: u64,
        tool_name: impl AsRef<str>,
        status: impl AsRef<str>,
        content: impl AsRef<str>,
    ) -> Self {
        Self::user(
            serde_json::json!({
                "type": "tool_result",
                "action_id": action_id,
                "tool": tool_name.as_ref(),
                "status": status.as_ref(),
                "content": content.as_ref(),
            })
            .to_string(),
        )
    }
}

/// A request input sequence that can contain both ordinary messages and
/// opaque Responses API items.
///
/// Keeping opaque items byte-for-byte representable is important for
/// stateless replay: future response item types (including compaction items)
/// must be sent back without this client having to understand their schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputItems(Vec<InputItem>);

impl InputItems {
    #[must_use]
    pub fn from_messages(messages: Vec<InputMessage>) -> Self {
        Self(messages.into_iter().map(InputItem::Message).collect())
    }

    #[must_use]
    pub fn from_opaque(items: Vec<Value>) -> Self {
        Self(items.into_iter().map(InputItem::Opaque).collect())
    }

    /// Preserve the existing message-oriented call site while still allowing
    /// opaque items through `push_opaque`.
    pub fn push(&mut self, message: InputMessage) {
        self.0.push(InputItem::Message(message));
    }

    pub fn push_opaque(&mut self, item: Value) {
        self.0.push(InputItem::Opaque(item));
    }

    /// Append the result of a native Responses API function call. The
    /// `call_id` is the causal link back to the matching `function_call`
    /// output item and must be preserved exactly.
    pub fn push_function_call_output(
        &mut self,
        call_id: impl Into<String>,
        output: impl Into<String>,
    ) {
        self.0
            .push(InputItem::FunctionCallOutput(FunctionCallOutput {
                item_type: FunctionCallOutputType::FunctionCallOutput,
                call_id: call_id.into(),
                output: output.into(),
            }));
    }

    pub fn extend_opaque(&mut self, items: impl IntoIterator<Item = Value>) {
        self.0.extend(items.into_iter().map(InputItem::Opaque));
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &InputItem> {
        self.0.iter()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.0.iter_mut().filter_map(|item| match item {
            InputItem::Opaque(value) => Some(value),
            InputItem::Message(_) | InputItem::FunctionCallOutput(_) => None,
        })
    }

    #[must_use]
    pub fn into_inner(self) -> Vec<InputItem> {
        self.0
    }
}

impl From<Vec<InputMessage>> for InputItems {
    fn from(messages: Vec<InputMessage>) -> Self {
        Self::from_messages(messages)
    }
}

impl From<Vec<Value>> for InputItems {
    fn from(items: Vec<Value>) -> Self {
        Self::from_opaque(items)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum InputItem {
    Message(InputMessage),
    FunctionCallOutput(FunctionCallOutput),
    Opaque(Value),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCallOutput {
    #[serde(rename = "type")]
    item_type: FunctionCallOutputType,
    pub call_id: String,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum FunctionCallOutputType {
    #[serde(rename = "function_call_output")]
    FunctionCallOutput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FunctionToolDefinition {
    #[serde(rename = "type")]
    tool_type: FunctionToolType,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
    pub strict: bool,
}

impl FunctionToolDefinition {
    #[must_use]
    pub fn new(name: impl Into<String>, description: Option<String>, parameters: Value) -> Self {
        Self {
            tool_type: FunctionToolType::Function,
            name: name.into(),
            description,
            parameters,
            strict: true,
        }
    }

    #[must_use]
    pub const fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum FunctionToolType {
    #[serde(rename = "function")]
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
}

/// Opaque server response item. The API intentionally evolves this union; a
/// transparent value prevents unknown item types or output roles from making
/// an otherwise valid terminal response fail deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OutputItem(pub Value);

impl OutputItem {
    #[must_use]
    pub const fn as_value(&self) -> &Value {
        &self.0
    }

    #[must_use]
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl From<Value> for OutputItem {
    fn from(value: Value) -> Self {
        Self(value)
    }
}

impl From<OutputItem> for Value {
    fn from(item: OutputItem) -> Self {
        item.0
    }
}

/// `context_management` entries are also an evolving API union. Treat them as
/// serde primitives so callers can opt into new server capabilities without a
/// client release.
pub type ContextManagement = Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResponsesRequest {
    pub model: String,
    pub instructions: String,
    pub input: InputItems,
    pub max_output_tokens: u32,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    pub store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_management: Option<Vec<ContextManagement>>,
    /// Extra response fields requested from the service. Stateless requests
    /// request encrypted reasoning content so opaque reasoning items can be
    /// replayed on the next turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include: Option<Vec<String>>,
    /// Native function tools use the Responses API item protocol. They are
    /// separate from the legacy XML-tag tools parsed from output text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<FunctionToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parallel_tool_calls: Option<bool>,
}

impl ResponsesRequest {
    pub fn new(
        model: impl Into<String>,
        instructions: impl Into<String>,
        input: Vec<InputMessage>,
        max_output_tokens: u32,
    ) -> Self {
        Self::stateless(model, instructions, input, max_output_tokens)
    }

    pub fn stateless(
        model: impl Into<String>,
        instructions: impl Into<String>,
        committed_history: impl Into<InputItems>,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            instructions: instructions.into(),
            input: committed_history.into(),
            max_output_tokens,
            stream: true,
            previous_response_id: None,
            store: false,
            reasoning: None,
            temperature: None,
            context_management: None,
            include: stateless_replay_includes(),
            tools: None,
            parallel_tool_calls: None,
        }
    }

    /// Construct a stateless request from an exact sequence of opaque API
    /// items. This is the lossless replay path for prior response output.
    pub fn stateless_replay(
        model: impl Into<String>,
        instructions: impl Into<String>,
        replay_input: Vec<Value>,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            instructions: instructions.into(),
            input: replay_input.into(),
            max_output_tokens,
            stream: true,
            previous_response_id: None,
            store: false,
            reasoning: None,
            temperature: None,
            context_management: None,
            include: stateless_replay_includes(),
            tools: None,
            parallel_tool_calls: None,
        }
    }

    /// Construct a stateful request. `new_input_only` must contain only items
    /// not already represented by `previous_response_id`.
    pub fn stateful(
        model: impl Into<String>,
        instructions: impl Into<String>,
        new_input_only: impl Into<InputItems>,
        max_output_tokens: u32,
        previous_response_id: impl Into<String>,
    ) -> Self {
        Self {
            model: model.into(),
            instructions: instructions.into(),
            input: new_input_only.into(),
            max_output_tokens,
            stream: true,
            previous_response_id: Some(previous_response_id.into()),
            store: true,
            reasoning: None,
            temperature: None,
            context_management: None,
            include: None,
            tools: None,
            parallel_tool_calls: None,
        }
    }

    pub fn with_reasoning(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning = Some(ReasoningConfig { effort, mode: None });
        self
    }

    #[must_use]
    pub fn with_reasoning_mode(
        mut self,
        effort: ReasoningEffort,
        mode: Option<ReasoningMode>,
    ) -> Self {
        self.reasoning = Some(ReasoningConfig { effort, mode });
        self
    }

    #[must_use]
    pub fn with_temperature(mut self, temperature: Option<f32>) -> Self {
        self.temperature = temperature;
        self
    }

    #[must_use]
    pub fn has_attachment_input(&self) -> bool {
        self.input.iter().any(|item| match item {
            InputItem::Opaque(value) => contains_attachment_part(value),
            InputItem::Message(_) | InputItem::FunctionCallOutput(_) => false,
        })
    }

    #[must_use]
    pub fn with_context_management(mut self, context_management: Vec<ContextManagement>) -> Self {
        self.context_management = (!context_management.is_empty()).then_some(context_management);
        self
    }

    #[must_use]
    pub fn with_include(mut self, include: Vec<String>) -> Self {
        self.include = (!include.is_empty()).then_some(include);
        self
    }

    #[must_use]
    pub fn with_tools(mut self, tools: Vec<FunctionToolDefinition>) -> Self {
        self.tools = (!tools.is_empty()).then_some(tools);
        self.parallel_tool_calls = self.tools.as_ref().map(|_| false);
        self
    }
}

fn contains_attachment_part(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_attachment_part),
        Value::Object(object) => {
            matches!(
                object.get("type").and_then(Value::as_str),
                Some("input_image" | "input_file" | "input_audio" | "input_video")
            ) || object.values().any(contains_attachment_part)
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn stateless_replay_includes() -> Option<Vec<String>> {
    Some(vec!["reasoning.encrypted_content".to_owned()])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningConfig {
    pub effort: ReasoningEffort,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ReasoningMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl ReasoningEffort {
    #[must_use]
    pub fn at_least(self, minimum: Self) -> Self {
        self.max(minimum)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningMode {
    Standard,
    Pro,
}

impl std::fmt::Display for ReasoningMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Standard => "standard",
            Self::Pro => "pro",
        })
    }
}

impl FromStr for ReasoningEffort {
    type Err = ParseReasoningEffortError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => Err(ParseReasoningEffortError {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("expected 'low', 'medium', 'high', 'xhigh', or 'max', got {value:?}")]
pub struct ParseReasoningEffortError {
    value: String,
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponsesResponse {
    pub id: String,
    #[serde(default)]
    pub status: Option<ResponseStatus>,
    #[serde(default)]
    pub output: Vec<OutputItem>,
    #[serde(default)]
    pub usage: Option<UsageStats>,
    /// The Responses API uses Unix epoch seconds, not an RFC 3339 string.
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub error: Option<ResponseError>,
}

impl ResponsesResponse {
    pub fn output_text(&self) -> String {
        self.output
            .iter()
            .filter_map(|item| item.0.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|content| content.get("type").and_then(Value::as_str) == Some("output_text"))
            .filter_map(|content| content.get("text").and_then(Value::as_str))
            .collect()
    }

    /// Clone the opaque output items for a subsequent stateless request.
    #[must_use]
    pub fn replay_items(&self) -> Vec<Value> {
        self.output.iter().map(|item| item.0.clone()).collect()
    }

    /// Extract native function calls from the completed response. Unknown
    /// output item types remain ignored for forward compatibility, while a
    /// malformed item that explicitly claims to be a function call is a hard
    /// protocol error rather than a silently dropped tool request.
    pub fn function_calls(&self) -> Result<Vec<FunctionCall>, ApiError> {
        self.output
            .iter()
            .filter(|item| item.0.get("type").and_then(Value::as_str) == Some("function_call"))
            .map(|item| {
                let call_id = required_string_field(&item.0, "call_id")?;
                let name = required_string_field(&item.0, "name")?;
                let arguments = required_string_field(&item.0, "arguments")?;
                Ok(FunctionCall {
                    call_id,
                    name,
                    arguments,
                })
            })
            .collect()
    }
}

fn required_string_field(item: &Value, field: &str) -> Result<String, ApiError> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ApiError::Protocol(format!(
                "function_call output item has missing or empty {field:?} field"
            ))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ResponseStatus {
    Completed,
    Failed,
    Cancelled,
    Incomplete,
    #[serde(other)]
    Unknown,
}

/// Typed output content remains available as a convenience primitive, while
/// `ResponsesResponse::output` stays opaque for forward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutputContent {
    #[serde(rename = "type")]
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseError {
    #[serde(default)]
    pub code: Option<String>,
    pub message: String,
    #[serde(default)]
    pub param: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageStats {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    #[serde(default)]
    pub input_tokens_details: Option<InputTokenDetails>,
}

impl UsageStats {
    #[must_use]
    pub fn cached_input_tokens(&self) -> u64 {
        self.input_tokens_details
            .as_ref()
            .map_or(0, |details| details.cached_tokens.min(self.input_tokens))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputTokenDetails {
    #[serde(default)]
    pub cached_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type")]
pub enum StreamEvent {
    #[serde(rename = "response.created")]
    Created { response: ResponsesResponse },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },
    #[serde(rename = "response.output_text.done")]
    OutputTextDone { text: String },
    #[serde(rename = "response.completed")]
    Completed { response: ResponsesResponse },
    #[serde(rename = "response.failed")]
    Failed { response: ResponsesResponse },
    #[serde(rename = "response.cancelled")]
    Cancelled {
        #[serde(default)]
        response: Option<ResponsesResponse>,
    },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponsesResponse },
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        code: Option<String>,
        message: String,
        #[serde(default)]
        param: Option<String>,
    },
    /// Synthetic marker emitted for an SSE `data: [DONE]` frame.
    #[serde(skip_deserializing)]
    Done,
    #[serde(other)]
    Ignored,
}

#[derive(Debug, Clone)]
pub struct CompletedResponse {
    pub response: ResponsesResponse,
    pub text: String,
    pub events: Vec<StreamEvent>,
}

/// A terminal success must be internally consistent before callers commit it.
pub fn validate_completed_status(response: &ResponsesResponse) -> Result<(), ApiError> {
    if response.status != Some(ResponseStatus::Completed) {
        return Err(ApiError::Protocol(format!(
            "response.completed carried non-completed status {:?}",
            response.status
        )));
    }
    if response.id.trim().is_empty() {
        return Err(ApiError::Protocol(
            "response.completed carried an empty response id".to_owned(),
        ));
    }
    if response.error.is_some() {
        return Err(ApiError::Protocol(
            "response.completed carried an error payload".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn opaque_output_roles_and_fields_round_trip_losslessly()
    -> Result<(), Box<dyn std::error::Error>> {
        let value = json!({
            "type": "message",
            "role": "future_role",
            "content": [{"type": "output_text", "text": "hello", "future": true}],
            "future_top_level": {"value": 7}
        });
        let item: OutputItem = serde_json::from_value(value.clone())?;
        assert_eq!(serde_json::to_value(&item)?, value);
        Ok(())
    }

    #[test]
    fn stateless_replay_and_context_management_are_opaque() -> Result<(), Box<dyn std::error::Error>>
    {
        let replay = json!({"type": "compaction", "encrypted_content": "opaque"});
        let context = json!({"type": "compaction", "compact_threshold": 32_000});
        let request =
            ResponsesRequest::stateless_replay("model", "instructions", vec![replay.clone()], 128)
                .with_context_management(vec![context.clone()]);
        let value = serde_json::to_value(request)?;
        assert_eq!(value["input"][0], replay);
        assert_eq!(value["context_management"][0], context);
        assert_eq!(value["include"][0], "reasoning.encrypted_content");
        Ok(())
    }

    #[test]
    fn completed_status_is_explicit_and_strict() -> Result<(), Box<dyn std::error::Error>> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "id": "r",
            "output": []
        }))?;
        assert!(validate_completed_status(&response).is_err());

        let contradictory: ResponsesResponse = serde_json::from_value(json!({
            "id": "r",
            "status": "completed",
            "output": [],
            "error": {"code": "server_error", "message": "failed"}
        }))?;
        assert!(validate_completed_status(&contradictory).is_err());

        let missing_id: ResponsesResponse = serde_json::from_value(json!({
            "id": "",
            "status": "completed",
            "output": []
        }))?;
        assert!(validate_completed_status(&missing_id).is_err());
        Ok(())
    }

    #[test]
    fn cached_input_usage_is_optional_and_clamped_to_total_input()
    -> Result<(), Box<dyn std::error::Error>> {
        let without_details: UsageStats = serde_json::from_value(json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15
        }))?;
        assert_eq!(without_details.cached_input_tokens(), 0);

        let malformed_provider_value: UsageStats = serde_json::from_value(json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15,
            "input_tokens_details": {"cached_tokens": 99}
        }))?;
        assert_eq!(malformed_provider_value.cached_input_tokens(), 10);
        Ok(())
    }

    #[test]
    fn responses_request_uses_input_and_native_function_items()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut input = InputItems::from_messages(vec![InputMessage::user("inspect")]);
        input.push_function_call_output("call_7", r#"{"files":3}"#);
        let mut request =
            ResponsesRequest::stateless("model", "instructions", Vec::<InputMessage>::new(), 128)
                .with_tools(vec![FunctionToolDefinition::new(
                    "mcp__files__list",
                    Some("List files".to_owned()),
                    json!({
                        "type": "object",
                        "properties": {},
                        "additionalProperties": false
                    }),
                )]);
        request.input = input;

        let value = serde_json::to_value(request)?;
        assert!(value.get("messages").is_none());
        assert_eq!(value["input"][0]["role"], "user");
        assert_eq!(value["input"][1]["type"], "function_call_output");
        assert_eq!(value["input"][1]["call_id"], "call_7");
        assert_eq!(value["tools"][0]["type"], "function");
        assert_eq!(value["tools"][0]["name"], "mcp__files__list");
        assert_eq!(value["parallel_tool_calls"], false);
        Ok(())
    }

    #[test]
    fn function_calls_are_strictly_extracted_from_opaque_output()
    -> Result<(), Box<dyn std::error::Error>> {
        let response: ResponsesResponse = serde_json::from_value(json!({
            "id": "r",
            "status": "completed",
            "output": [
                {"type": "future_item", "value": 1},
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "mcp__git__status",
                    "arguments": "{\"short\":true}"
                }
            ]
        }))?;
        assert_eq!(
            response.function_calls()?,
            vec![FunctionCall {
                call_id: "call_1".to_owned(),
                name: "mcp__git__status".to_owned(),
                arguments: "{\"short\":true}".to_owned(),
            }]
        );

        let malformed: ResponsesResponse = serde_json::from_value(json!({
            "id": "bad",
            "status": "completed",
            "output": [{"type": "function_call", "call_id": "call_2", "arguments": "{}"}]
        }))?;
        assert!(malformed.function_calls().is_err());
        Ok(())
    }
}
