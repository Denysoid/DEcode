use std::collections::BTreeMap;

use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream::BoxStream};
use serde_json::Value;

use super::{
    stream::parse_sse_data_stream,
    types::{
        InputItem, InputTokenDetails, OutputItem, ResponseError, ResponseStatus, ResponsesRequest,
        ResponsesResponse, StreamEvent, UsageStats,
    },
};
use crate::{config::ApiWireProtocol, error::ApiError};

pub(crate) fn encode_request(
    request: &ResponsesRequest,
    protocol: ApiWireProtocol,
) -> Result<Value, ApiError> {
    match protocol {
        ApiWireProtocol::Responses => serde_json::to_value(request)
            .map_err(|error| ApiError::Protocol(format!("request serialization failed: {error}"))),
        ApiWireProtocol::ChatCompletions => encode_chat_request(request),
        ApiWireProtocol::AnthropicMessages => encode_anthropic_request(request),
        ApiWireProtocol::GeminiGenerateContent => encode_gemini_request(request),
    }
}

pub(crate) fn parse_provider_stream<S>(
    byte_stream: S,
    protocol: ApiWireProtocol,
) -> BoxStream<'static, Result<StreamEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    match protocol {
        ApiWireProtocol::Responses => super::stream::parse_sse_stream(byte_stream),
        ApiWireProtocol::ChatCompletions => parse_chat_stream(byte_stream),
        ApiWireProtocol::AnthropicMessages => parse_anthropic_stream(byte_stream),
        ApiWireProtocol::GeminiGenerateContent => parse_gemini_stream(byte_stream),
    }
}

fn encode_gemini_request(request: &ResponsesRequest) -> Result<Value, ApiError> {
    let mut contents = Vec::new();
    let mut system_parts = vec![serde_json::json!({"text": request.instructions})];
    for item in request.input.iter() {
        match item {
            InputItem::Message(message)
                if matches!(
                    message.role,
                    super::types::Role::System | super::types::Role::Developer
                ) =>
            {
                system_parts.push(serde_json::json!({"text": message.content}));
            }
            _ => append_gemini_item(&mut contents, item)?,
        }
    }
    let mut body = serde_json::json!({
        "systemInstruction": {"parts": system_parts},
        "contents": contents,
        "generationConfig": {"maxOutputTokens": request.max_output_tokens},
    });
    let object = body
        .as_object_mut()
        .ok_or_else(|| ApiError::Protocol("Gemini request was not an object".to_owned()))?;
    if let Some(temperature) = request.temperature {
        object["generationConfig"]["temperature"] = serde_json::json!(temperature);
    }
    if let Some(tools) = request.tools.as_ref().filter(|tools| !tools.is_empty()) {
        let declarations = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
            })
            .collect::<Vec<_>>();
        object.insert(
            "tools".to_owned(),
            serde_json::json!([{"functionDeclarations": declarations}]),
        );
    }
    Ok(body)
}

fn append_gemini_item(contents: &mut Vec<Value>, item: &InputItem) -> Result<(), ApiError> {
    match item {
        InputItem::Message(message) => contents.push(serde_json::json!({
            "role": if message.role == super::types::Role::Assistant { "model" } else { "user" },
            "parts": [{"text": message.content}],
        })),
        InputItem::FunctionCallOutput(output) => {
            contents.push(gemini_function_response(&output.call_id, &output.output))
        }
        InputItem::Opaque(value) => append_gemini_opaque(contents, value)?,
    }
    Ok(())
}

fn append_gemini_opaque(contents: &mut Vec<Value>, value: &Value) -> Result<(), ApiError> {
    if let Some(role) = value.get("role").and_then(Value::as_str) {
        let content = value.get("content").cloned().unwrap_or(Value::Null);
        contents.push(serde_json::json!({
            "role": if role == "assistant" { "model" } else { "user" },
            "parts": gemini_parts(content)?,
        }));
        return Ok(());
    }
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {
            let parts = value
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .map(|text| serde_json::json!({"text": text}))
                .collect::<Vec<_>>();
            if !parts.is_empty() {
                contents.push(serde_json::json!({"role":"model", "parts":parts}));
            }
        }
        Some("function_call") => {
            let arguments = parse_tool_arguments(value)?;
            contents.push(serde_json::json!({
                "role": "model",
                "parts": [{"functionCall": {
                    "name": required(value, "name")?,
                    "args": arguments,
                }}],
            }));
        }
        Some("function_call_output") => contents.push(gemini_function_response(
            required(value, "call_id")?,
            required(value, "output")?,
        )),
        _ => {}
    }
    Ok(())
}

fn gemini_function_response(call_id: &str, output: &str) -> Value {
    let name = call_id
        .strip_prefix("gemini:")
        .and_then(|value| value.rsplit_once(':').map(|(name, _)| name))
        .unwrap_or(call_id);
    serde_json::json!({
        "role": "user",
        "parts": [{"functionResponse": {
            "name": name,
            "response": {"result": output},
        }}],
    })
}

fn gemini_parts(content: Value) -> Result<Vec<Value>, ApiError> {
    let Value::Array(parts) = content else {
        return Ok(vec![serde_json::json!({"text": content})]);
    };
    let mut converted = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => {
                converted.push(serde_json::json!({"text": required(&part, "text")?}));
            }
            Some("input_image") => {
                converted.push(gemini_inline_data(required(&part, "image_url")?)?)
            }
            Some("input_file") => {
                converted.push(gemini_inline_data(required(&part, "file_data")?)?)
            }
            Some("input_audio") => {
                converted.push(gemini_inline_data(required(&part, "audio_data")?)?)
            }
            Some("input_video") => {
                converted.push(gemini_inline_data(required(&part, "video_data")?)?)
            }
            _ => {}
        }
    }
    Ok(converted)
}

fn gemini_inline_data(data_url: &str) -> Result<Value, ApiError> {
    let (mime_type, data) = split_data_url(data_url)?;
    Ok(serde_json::json!({
        "inlineData": {"mimeType": mime_type, "data": data}
    }))
}

fn encode_chat_request(request: &ResponsesRequest) -> Result<Value, ApiError> {
    let mut messages = vec![serde_json::json!({
        "role": "system",
        "content": request.instructions,
    })];
    for item in request.input.iter() {
        append_chat_item(&mut messages, item)?;
    }
    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                        "strict": tool.strict,
                    }
                })
            })
            .collect::<Vec<_>>()
    });
    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": request.max_output_tokens,
    });
    let object = body
        .as_object_mut()
        .ok_or_else(|| ApiError::Protocol("chat request was not an object".to_owned()))?;
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_owned(), serde_json::json!(temperature));
    }
    if let Some(reasoning) = &request.reasoning {
        object.insert(
            "reasoning_effort".to_owned(),
            serde_json::json!(reasoning.effort.to_string()),
        );
    }
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        object.insert("tools".to_owned(), Value::Array(tools));
        object.insert("parallel_tool_calls".to_owned(), Value::Bool(false));
    }
    Ok(body)
}

fn append_chat_item(messages: &mut Vec<Value>, item: &InputItem) -> Result<(), ApiError> {
    match item {
        InputItem::Message(message) => messages.push(serde_json::json!({
            "role": serde_json::to_value(message.role).map_err(|error| ApiError::Protocol(error.to_string()))?,
            "content": message.content,
        })),
        InputItem::FunctionCallOutput(output) => messages.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": output.call_id,
            "content": output.output,
        })),
        InputItem::Opaque(value) => append_chat_opaque(messages, value)?,
    }
    Ok(())
}

fn append_chat_opaque(messages: &mut Vec<Value>, value: &Value) -> Result<(), ApiError> {
    if value.get("role").and_then(Value::as_str).is_some() {
        let role = value.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = value.get("content").cloned().unwrap_or(Value::Null);
        messages.push(serde_json::json!({
            "role": role,
            "content": chat_content(content)?,
        }));
        return Ok(());
    }
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text = value
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>();
            if !text.is_empty() {
                messages.push(serde_json::json!({"role":"assistant","content":text}));
            }
        }
        Some("function_call") => {
            let call_id = required(value, "call_id")?;
            let name = required(value, "name")?;
            let arguments = required(value, "arguments")?;
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": Value::Null,
                "tool_calls": [{
                    "id": call_id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments}
                }]
            }));
        }
        Some("function_call_output") => {
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": required(value, "call_id")?,
                "content": required(value, "output")?,
            }));
        }
        _ => {}
    }
    Ok(())
}

fn chat_content(value: Value) -> Result<Value, ApiError> {
    let Value::Array(parts) = value else {
        return Ok(value);
    };
    let mut converted = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => converted.push(serde_json::json!({
                "type": "text",
                "text": required(&part, "text")?,
            })),
            Some("input_image") => converted.push(serde_json::json!({
                "type": "image_url",
                "image_url": {
                    "url": required(&part, "image_url")?,
                    "detail": part.get("detail").and_then(Value::as_str).unwrap_or("auto"),
                }
            })),
            Some("input_audio") => {
                let data_url = required(&part, "audio_data")?;
                let data = data_url.split_once(',').map_or(data_url, |(_, data)| data);
                converted.push(serde_json::json!({
                    "type": "input_audio",
                    "input_audio": {
                        "data": data,
                        "format": part.get("format").and_then(Value::as_str).unwrap_or("mp3"),
                    }
                }));
            }
            Some("input_video") => converted.push(serde_json::json!({
                "type": "video_url",
                "video_url": {"url": required(&part, "video_data")?}
            })),
            Some("input_file") => {
                return Err(ApiError::Protocol(
                    "the selected Chat Completions provider cannot represent input_file; use a Responses provider or provider-native file upload"
                        .to_owned(),
                ));
            }
            _ => {}
        }
    }
    Ok(Value::Array(converted))
}

fn encode_anthropic_request(request: &ResponsesRequest) -> Result<Value, ApiError> {
    let mut messages = Vec::new();
    let mut system = request.instructions.clone();
    for item in request.input.iter() {
        match item {
            InputItem::Message(message)
                if matches!(
                    message.role,
                    super::types::Role::System | super::types::Role::Developer
                ) =>
            {
                if !system.is_empty() && !message.content.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(&message.content);
            }
            _ => append_anthropic_item(&mut messages, item)?,
        }
    }
    let tools = request.tools.as_ref().map(|tools| {
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect::<Vec<_>>()
    });
    let mut body = serde_json::json!({
        "model": request.model,
        "system": system,
        "messages": messages,
        "stream": true,
        "max_tokens": request.max_output_tokens,
    });
    let object = body
        .as_object_mut()
        .ok_or_else(|| ApiError::Protocol("Anthropic request was not an object".to_owned()))?;
    if let Some(temperature) = request.temperature {
        object.insert("temperature".to_owned(), serde_json::json!(temperature));
    }
    if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
        object.insert("tools".to_owned(), Value::Array(tools));
        object.insert("disable_parallel_tool_use".to_owned(), Value::Bool(true));
    }
    Ok(body)
}

fn append_anthropic_item(messages: &mut Vec<Value>, item: &InputItem) -> Result<(), ApiError> {
    match item {
        InputItem::Message(message) => messages.push(serde_json::json!({
            "role": match message.role {
                super::types::Role::Assistant => "assistant",
                super::types::Role::System | super::types::Role::Developer | super::types::Role::User => "user",
            },
            "content": message.content,
        })),
        InputItem::FunctionCallOutput(output) => messages.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": output.call_id,
                "content": output.output,
            }]
        })),
        InputItem::Opaque(value) => append_anthropic_opaque(messages, value)?,
    }
    Ok(())
}

fn append_anthropic_opaque(messages: &mut Vec<Value>, value: &Value) -> Result<(), ApiError> {
    if let Some(role) = value.get("role").and_then(Value::as_str) {
        let role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        let content = value.get("content").cloned().unwrap_or(Value::Null);
        messages.push(serde_json::json!({
            "role": role,
            "content": anthropic_content(content)?,
        }));
        return Ok(());
    }
    match value.get("type").and_then(Value::as_str) {
        Some("message") => {
            let text = value
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<String>();
            if !text.is_empty() {
                messages.push(serde_json::json!({"role":"assistant","content":text}));
            }
        }
        Some("function_call") => {
            let arguments = parse_tool_arguments(value)?;
            messages.push(serde_json::json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": required(value, "call_id")?,
                    "name": required(value, "name")?,
                    "input": arguments,
                }]
            }));
        }
        Some("function_call_output") => messages.push(serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": required(value, "call_id")?,
                "content": required(value, "output")?,
            }]
        })),
        _ => {}
    }
    Ok(())
}

fn anthropic_content(value: Value) -> Result<Value, ApiError> {
    let Value::Array(parts) = value else {
        return Ok(value);
    };
    let mut converted = Vec::with_capacity(parts.len());
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text" | "text") => converted.push(serde_json::json!({
                "type": "text",
                "text": required(&part, "text")?,
            })),
            Some("input_image") => {
                let (media_type, data) = split_data_url(required(&part, "image_url")?)?;
                converted.push(serde_json::json!({
                    "type": "image",
                    "source": {"type":"base64", "media_type":media_type, "data":data}
                }));
            }
            Some("input_file") => {
                let (media_type, data) = split_data_url(required(&part, "file_data")?)?;
                converted.push(serde_json::json!({
                    "type": "document",
                    "source": {"type":"base64", "media_type":media_type, "data":data},
                    "title": part.get("filename").and_then(Value::as_str),
                }));
            }
            _ => {}
        }
    }
    Ok(Value::Array(converted))
}

fn parse_chat_stream<S>(byte_stream: S) -> BoxStream<'static, Result<StreamEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut raw = parse_sse_data_stream(byte_stream);
        let mut accumulator = ChatAccumulator::default();
        while let Some(item) = raw.next().await {
            let data = match item {
                Ok(data) => data,
                Err(error) => { yield Err(error); return; }
            };
            if data.trim() == "[DONE]" {
                yield accumulator.finish_event();
                return;
            }
            let value: Value = match serde_json::from_str(&data) {
                Ok(value) => value,
                Err(error) => {
                    yield Err(ApiError::Protocol(format!("malformed Chat Completions SSE JSON: {error}")));
                    return;
                }
            };
            if let Some(error) = provider_error(&value) {
                yield Ok(StreamEvent::Error { code: error.code, message: error.message, param: error.param });
                return;
            }
            accumulator.ingest(&value);
            for delta in chat_text_deltas(&value) {
                yield Ok(StreamEvent::OutputTextDelta { delta });
            }
        }
        if accumulator.finish_reason.is_some() {
            yield accumulator.finish_event();
        }
    })
}

fn parse_anthropic_stream<S>(byte_stream: S) -> BoxStream<'static, Result<StreamEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut raw = parse_sse_data_stream(byte_stream);
        let mut accumulator = AnthropicAccumulator::default();
        while let Some(item) = raw.next().await {
            let data = match item {
                Ok(data) => data,
                Err(error) => { yield Err(error); return; }
            };
            let value: Value = match serde_json::from_str(&data) {
                Ok(value) => value,
                Err(error) => {
                    yield Err(ApiError::Protocol(format!("malformed Anthropic SSE JSON: {error}")));
                    return;
                }
            };
            if let Some(error) = provider_error(&value) {
                yield Ok(StreamEvent::Error { code: error.code, message: error.message, param: error.param });
                return;
            }
            if let Some(delta) = anthropic_text_delta(&value) {
                accumulator.text.push_str(&delta);
                yield Ok(StreamEvent::OutputTextDelta { delta });
            }
            accumulator.ingest(&value);
            if value.get("type").and_then(Value::as_str) == Some("message_stop") {
                yield accumulator.finish_event();
                return;
            }
        }
    })
}

fn parse_gemini_stream<S>(byte_stream: S) -> BoxStream<'static, Result<StreamEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut raw = parse_sse_data_stream(byte_stream);
        let mut accumulator = GeminiAccumulator::default();
        while let Some(item) = raw.next().await {
            let data = match item {
                Ok(data) => data,
                Err(error) => { yield Err(error); return; }
            };
            let value: Value = match serde_json::from_str(&data) {
                Ok(value) => value,
                Err(error) => {
                    yield Err(ApiError::Protocol(format!("malformed Gemini SSE JSON: {error}")));
                    return;
                }
            };
            if let Some(error) = provider_error(&value) {
                yield Ok(StreamEvent::Error { code: error.code, message: error.message, param: error.param });
                return;
            }
            for delta in gemini_text_deltas(&value) {
                yield Ok(StreamEvent::OutputTextDelta { delta });
            }
            accumulator.ingest(&value);
        }
        if accumulator.saw_response {
            yield accumulator.finish_event();
        }
    })
}

#[derive(Default)]
struct ToolAccumulator {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Default)]
struct ChatAccumulator {
    id: String,
    text: String,
    tools: BTreeMap<usize, ToolAccumulator>,
    usage: Option<UsageStats>,
    finish_reason: Option<String>,
}

impl ChatAccumulator {
    fn ingest(&mut self, value: &Value) {
        if self.id.is_empty()
            && let Some(id) = value.get("id").and_then(Value::as_str)
        {
            self.id = id.to_owned();
        }
        for delta in chat_text_deltas(value) {
            self.text.push_str(&delta);
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                if self.finish_reason.is_none()
                    && let Some(reason) = choice.get("finish_reason").and_then(Value::as_str)
                {
                    self.finish_reason = Some(reason.to_owned());
                }
                let Some(tool_calls) = choice
                    .get("delta")
                    .and_then(|delta| delta.get("tool_calls"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for call in tool_calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let tool = self.tools.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        tool.id = id.to_owned();
                    }
                    if let Some(function) = call.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            tool.name.push_str(name);
                        }
                        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                            tool.arguments.push_str(arguments);
                        }
                    }
                }
            }
        }
        if let Some(usage) = value.get("usage") {
            let input_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_tokens = usage
                .get("total_tokens")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
            let cached_tokens = usage
                .get("prompt_tokens_details")
                .and_then(|details| details.get("cached_tokens"))
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.usage = Some(UsageStats {
                input_tokens,
                output_tokens,
                total_tokens,
                input_tokens_details: Some(InputTokenDetails { cached_tokens }),
            });
        }
    }

    fn finish_event(self) -> Result<StreamEvent, ApiError> {
        let reason = self.finish_reason.clone().ok_or_else(|| {
            ApiError::Protocol("Chat Completions stream ended without a finish reason".to_owned())
        })?;
        let response = completed_response(self.id, self.text, self.tools, self.usage);
        Ok(match reason.as_str() {
            "stop" | "tool_calls" | "function_call" => {
                response_event(response, ResponseStatus::Completed, None)
            }
            "length" => response_event(
                response,
                ResponseStatus::Incomplete,
                Some((
                    "chat_length",
                    "Chat Completions reached its output token limit",
                )),
            ),
            "content_filter" => response_event(
                response,
                ResponseStatus::Failed,
                Some((
                    "chat_content_filter",
                    "Chat Completions blocked the response",
                )),
            ),
            _ => response_event(
                response,
                ResponseStatus::Incomplete,
                Some((
                    "chat_unknown_finish_reason",
                    "Chat Completions returned an unrecognized finish reason",
                )),
            ),
        })
    }
}

#[derive(Default)]
struct AnthropicAccumulator {
    id: String,
    text: String,
    tools: BTreeMap<usize, ToolAccumulator>,
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    stop_reason: Option<String>,
}

#[derive(Default)]
struct GeminiAccumulator {
    text: String,
    tools: BTreeMap<usize, ToolAccumulator>,
    usage: Option<UsageStats>,
    saw_response: bool,
    finish_reason: Option<String>,
    block_reason: Option<String>,
}

impl GeminiAccumulator {
    fn ingest(&mut self, value: &Value) {
        self.saw_response = true;
        if self.block_reason.is_none() {
            self.block_reason = value
                .get("promptFeedback")
                .and_then(|feedback| feedback.get("blockReason"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        for text in gemini_text_deltas(value) {
            self.text.push_str(&text);
        }
        for candidate in value
            .get("candidates")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if self.finish_reason.is_none() {
                self.finish_reason = candidate
                    .get("finishReason")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }
            for part in candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(call) = part.get("functionCall") else {
                    continue;
                };
                let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let index = self.tools.len();
                let tool = self.tools.entry(index).or_default();
                tool.id = format!("gemini:{name}:{index}");
                tool.name = name.to_owned();
                tool.arguments = call
                    .get("args")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}))
                    .to_string();
            }
        }
        if let Some(usage) = value.get("usageMetadata") {
            let input_tokens = usage
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output_tokens = usage
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let total_tokens = usage
                .get("totalTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or_else(|| input_tokens.saturating_add(output_tokens));
            let cached_tokens = usage
                .get("cachedContentTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.usage = Some(UsageStats {
                input_tokens,
                output_tokens,
                total_tokens,
                input_tokens_details: Some(InputTokenDetails { cached_tokens }),
            });
        }
    }

    fn finish_event(self) -> Result<StreamEvent, ApiError> {
        let finish_reason = self.finish_reason.clone();
        let block_reason = self.block_reason.clone();
        let response = completed_response(
            "gemini-response".to_owned(),
            self.text,
            self.tools,
            self.usage,
        );
        if block_reason.is_some() {
            return Ok(response_event(
                response,
                ResponseStatus::Failed,
                Some(("gemini_prompt_blocked", "Gemini blocked the prompt")),
            ));
        }
        let reason = finish_reason.ok_or_else(|| {
            ApiError::Protocol("Gemini stream ended without a finish reason".to_owned())
        })?;
        Ok(match reason.as_str() {
            "STOP" => response_event(response, ResponseStatus::Completed, None),
            "MAX_TOKENS" => response_event(
                response,
                ResponseStatus::Incomplete,
                Some(("gemini_max_tokens", "Gemini reached its output token limit")),
            ),
            "SAFETY"
            | "RECITATION"
            | "LANGUAGE"
            | "BLOCKLIST"
            | "PROHIBITED_CONTENT"
            | "SPII"
            | "MALFORMED_FUNCTION_CALL"
            | "IMAGE_SAFETY"
            | "UNEXPECTED_TOOL_CALL"
            | "TOO_MANY_TOOL_CALLS" => response_event(
                response,
                ResponseStatus::Failed,
                Some(("gemini_generation_blocked", "Gemini blocked the response")),
            ),
            _ => response_event(
                response,
                ResponseStatus::Incomplete,
                Some((
                    "gemini_unknown_finish_reason",
                    "Gemini returned an unrecognized finish reason",
                )),
            ),
        })
    }
}

impl AnthropicAccumulator {
    fn ingest(&mut self, value: &Value) {
        match value.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if let Some(message) = value.get("message") {
                    self.id = message
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(usage) = message.get("usage") {
                        let uncached = usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        let cache_creation = usage
                            .get("cache_creation_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        self.cached_input_tokens = usage
                            .get("cache_read_input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0);
                        self.input_tokens = uncached
                            .saturating_add(cache_creation)
                            .saturating_add(self.cached_input_tokens);
                    }
                }
            }
            Some("content_block_start") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(block) = value.get("content_block")
                    && block.get("type").and_then(Value::as_str) == Some("tool_use")
                {
                    let tool = self.tools.entry(index).or_default();
                    tool.id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    tool.name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(input) = block.get("input").filter(|input| {
                        !input.is_null()
                            && !input.as_object().is_some_and(serde_json::Map::is_empty)
                    }) {
                        tool.arguments = input.to_string();
                    }
                }
            }
            Some("content_block_delta") => {
                let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if let Some(json) = value
                    .get("delta")
                    .filter(|delta| {
                        delta.get("type").and_then(Value::as_str) == Some("input_json_delta")
                    })
                    .and_then(|delta| delta.get("partial_json"))
                    .and_then(Value::as_str)
                {
                    self.tools
                        .entry(index)
                        .or_default()
                        .arguments
                        .push_str(json);
                }
            }
            Some("message_delta") => {
                if self.stop_reason.is_none() {
                    self.stop_reason = value
                        .get("delta")
                        .and_then(|delta| delta.get("stop_reason"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                self.output_tokens = value
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(self.output_tokens);
            }
            _ => {}
        }
    }

    fn finish_event(self) -> Result<StreamEvent, ApiError> {
        let reason = self.stop_reason.clone().ok_or_else(|| {
            ApiError::Protocol("Anthropic stream ended without a stop reason".to_owned())
        })?;
        let total_tokens = self.input_tokens.saturating_add(self.output_tokens);
        let response = completed_response(
            self.id,
            self.text,
            self.tools,
            Some(UsageStats {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                total_tokens,
                input_tokens_details: Some(InputTokenDetails {
                    cached_tokens: self.cached_input_tokens,
                }),
            }),
        );
        Ok(match reason.as_str() {
            "end_turn" | "stop_sequence" | "tool_use" => {
                response_event(response, ResponseStatus::Completed, None)
            }
            "max_tokens" | "model_context_window_exceeded" | "pause_turn" => response_event(
                response,
                ResponseStatus::Incomplete,
                Some((
                    "anthropic_incomplete",
                    "Anthropic stopped before completing the response",
                )),
            ),
            "refusal" => response_event(
                response,
                ResponseStatus::Failed,
                Some(("anthropic_refusal", "Anthropic refused the response")),
            ),
            _ => response_event(
                response,
                ResponseStatus::Incomplete,
                Some((
                    "anthropic_unknown_stop_reason",
                    "Anthropic returned an unrecognized stop reason",
                )),
            ),
        })
    }
}

fn completed_response(
    id: String,
    text: String,
    tools: BTreeMap<usize, ToolAccumulator>,
    usage: Option<UsageStats>,
) -> ResponsesResponse {
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(OutputItem(serde_json::json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type":"output_text", "text":text}],
        })));
    }
    output.extend(tools.into_values().map(|tool| {
        OutputItem(serde_json::json!({
            "type": "function_call",
            "call_id": tool.id,
            "name": tool.name,
            "arguments": if tool.arguments.is_empty() { "{}" } else { &tool.arguments },
        }))
    }));
    ResponsesResponse {
        id: if id.is_empty() {
            "provider-response".to_owned()
        } else {
            id
        },
        status: Some(ResponseStatus::Completed),
        output,
        usage,
        created_at: None,
        error: None,
    }
}

fn response_event(
    mut response: ResponsesResponse,
    status: ResponseStatus,
    error: Option<(&str, &str)>,
) -> StreamEvent {
    response.status = Some(status.clone());
    response.error = error.map(|(code, message)| ResponseError {
        code: Some(code.to_owned()),
        message: message.to_owned(),
        param: None,
    });
    match status {
        ResponseStatus::Completed => StreamEvent::Completed { response },
        ResponseStatus::Failed => StreamEvent::Failed { response },
        ResponseStatus::Incomplete => StreamEvent::Incomplete { response },
        ResponseStatus::Cancelled | ResponseStatus::Unknown => unreachable!(),
    }
}

fn chat_text_deltas(value: &Value) -> Vec<String> {
    value
        .get("choices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|choice| {
            choice
                .get("delta")
                .and_then(|delta| delta.get("content"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect()
}

fn gemini_text_deltas(value: &Value) -> Vec<String> {
    value
        .get("candidates")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|candidate| {
            candidate
                .get("content")
                .and_then(|content| content.get("parts"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
        })
        .filter(|part| part.get("thought").and_then(Value::as_bool) != Some(true))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect()
}

fn anthropic_text_delta(value: &Value) -> Option<String> {
    value
        .get("delta")
        .filter(|delta| delta.get("type").and_then(Value::as_str) == Some("text_delta"))
        .and_then(|delta| delta.get("text"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn provider_error(value: &Value) -> Option<ResponseError> {
    let error = value.get("error")?;
    Some(ResponseError {
        code: error
            .get("type")
            .or_else(|| error.get("code"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("provider returned an unspecified error")
            .to_owned(),
        param: error
            .get("param")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::Protocol(format!("provider replay item is missing {field}")))
}

fn parse_tool_arguments(value: &Value) -> Result<Value, ApiError> {
    let arguments = required(value, "arguments")?;
    serde_json::from_str(arguments).map_err(|error| {
        ApiError::Protocol(format!(
            "provider tool arguments are not valid JSON: {error}"
        ))
    })
}

fn split_data_url(value: &str) -> Result<(&str, &str), ApiError> {
    let value = value
        .strip_prefix("data:")
        .ok_or_else(|| ApiError::Protocol("attachment is not a data URL".to_owned()))?;
    let (metadata, data) = value
        .split_once(',')
        .ok_or_else(|| ApiError::Protocol("attachment data URL is malformed".to_owned()))?;
    let media_type = metadata
        .strip_suffix(";base64")
        .ok_or_else(|| ApiError::Protocol("attachment data URL is not base64".to_owned()))?;
    Ok((media_type, data))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::{StreamExt as _, stream};

    use super::*;
    use crate::api::types::{FunctionToolDefinition, InputMessage};

    #[test]
    fn chat_request_translates_input_and_tools() -> Result<(), ApiError> {
        let request = ResponsesRequest::stateless(
            "gemini",
            "be precise",
            vec![InputMessage::user("hello")],
            512,
        )
        .with_tools(vec![FunctionToolDefinition::new(
            "read_file",
            Some("read".to_owned()),
            serde_json::json!({"type":"object","properties":{}}),
        )]);
        let body = encode_request(&request, ApiWireProtocol::ChatCompletions)?;
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["content"], "hello");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        Ok(())
    }

    #[test]
    fn gemini_request_preserves_native_video_attachment() -> Result<(), ApiError> {
        let request = ResponsesRequest::stateless(
            "gemini-2.5-pro",
            "inspect the clip",
            vec![serde_json::json!({
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "What happens?"},
                    {
                        "type": "input_video",
                        "video_data": "data:video/mp4;base64,AAEC"
                    }
                ]
            })],
            512,
        );

        let body = encode_request(&request, ApiWireProtocol::GeminiGenerateContent)?;
        assert_eq!(body["contents"][0]["parts"][0]["text"], "What happens?");
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
            "video/mp4"
        );
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["data"],
            "AAEC"
        );
        Ok(())
    }

    #[tokio::test]
    async fn chat_stream_becomes_canonical_completed_response() {
        let body = concat!(
            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
            "data: [DONE]\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::ChatCompletions,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(
            matches!(events.first(), Some(Ok(StreamEvent::OutputTextDelta { delta })) if delta == "hi")
        );
        assert!(
            matches!(events.last(), Some(Ok(StreamEvent::Completed { response })) if response.output_text() == "hi")
        );
    }

    #[tokio::test]
    async fn chat_length_limit_is_incomplete() {
        let body = concat!(
            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::ChatCompletions,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Incomplete { response }))
                if response.status == Some(ResponseStatus::Incomplete)
        ));
    }

    #[tokio::test]
    async fn chat_done_without_finish_reason_is_not_completed() {
        let body = concat!(
            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::ChatCompletions,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Completed { .. })))
        );
    }

    #[tokio::test]
    async fn anthropic_token_limit_is_incomplete() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"usage\":{\"input_tokens\":3}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::AnthropicMessages,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Incomplete { response }))
                if response.status == Some(ResponseStatus::Incomplete)
        ));
    }

    #[tokio::test]
    async fn anthropic_usage_includes_cached_and_cache_creation_tokens() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"usage\":{\"input_tokens\":3,\"cache_creation_input_tokens\":2,\"cache_read_input_tokens\":5}}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::AnthropicMessages,
        )
        .collect::<Vec<_>>()
        .await;
        let usage = events.iter().find_map(|event| match event {
            Ok(StreamEvent::Completed { response }) => response.usage.as_ref(),
            _ => None,
        });
        assert_eq!(usage.map(|usage| usage.input_tokens), Some(10));
        assert_eq!(usage.map(UsageStats::cached_input_tokens), Some(5));
        assert_eq!(usage.map(|usage| usage.total_tokens), Some(14));
    }

    #[tokio::test]
    async fn gemini_token_limit_is_incomplete() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"partial\"}]}}]}\n\n",
            "data: {\"candidates\":[{\"finishReason\":\"MAX_TOKENS\"}]}\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::GeminiGenerateContent,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Incomplete { response }))
                if response.status == Some(ResponseStatus::Incomplete)
        ));
    }

    #[tokio::test]
    async fn gemini_prompt_block_is_failed() {
        let body = "data: {\"promptFeedback\":{\"blockReason\":\"SAFETY\"}}\n\n";
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::GeminiGenerateContent,
        )
        .collect::<Vec<_>>()
        .await;
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Failed { response }))
                if response.error.as_ref().and_then(|error| error.code.as_deref())
                    == Some("gemini_prompt_blocked")
        ));
    }

    #[tokio::test]
    async fn gemini_thought_parts_are_not_exposed_as_answer_text() {
        let body = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hidden\",\"thought\":true},{\"text\":\"visible\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::GeminiGenerateContent,
        )
        .collect::<Vec<_>>()
        .await;
        let response = events.iter().find_map(|event| match event {
            Ok(StreamEvent::Completed { response }) => Some(response),
            _ => None,
        });
        assert_eq!(
            response.map(ResponsesResponse::output_text).as_deref(),
            Some("visible")
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            Ok(StreamEvent::OutputTextDelta { delta }) if delta == "hidden"
        )));
    }

    #[test]
    fn native_replay_rejects_malformed_tool_arguments() {
        let request = ResponsesRequest::stateless_replay(
            "model",
            "system",
            vec![serde_json::json!({
                "type": "function_call",
                "call_id": "call-1",
                "name": "read_file",
                "arguments": "{broken"
            })],
            128,
        );
        assert!(encode_request(&request, ApiWireProtocol::AnthropicMessages).is_err());
        assert!(encode_request(&request, ApiWireProtocol::GeminiGenerateContent).is_err());
    }

    #[test]
    fn compatible_replay_preserves_assistant_output_text() -> Result<(), ApiError> {
        let request = ResponsesRequest::stateless_replay(
            "model",
            "system",
            vec![serde_json::json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "previous answer"}]
            })],
            128,
        );

        let chat = encode_request(&request, ApiWireProtocol::ChatCompletions)?;
        assert_eq!(chat["messages"][1]["content"][0]["text"], "previous answer");
        let anthropic = encode_request(&request, ApiWireProtocol::AnthropicMessages)?;
        assert_eq!(
            anthropic["messages"][0]["content"][0]["text"],
            "previous answer"
        );
        let gemini = encode_request(&request, ApiWireProtocol::GeminiGenerateContent)?;
        assert_eq!(gemini["contents"][0]["parts"][0]["text"], "previous answer");
        Ok(())
    }

    #[test]
    fn native_encoders_keep_system_messages_out_of_user_history() -> Result<(), ApiError> {
        let request = ResponsesRequest::stateless(
            "model",
            "primary",
            vec![
                InputMessage::developer("secondary"),
                InputMessage::user("hello"),
            ],
            128,
        );

        let anthropic = encode_request(&request, ApiWireProtocol::AnthropicMessages)?;
        assert_eq!(anthropic["system"], "primary\n\nsecondary");
        assert_eq!(anthropic["messages"].as_array().map(Vec::len), Some(1));
        assert_eq!(anthropic["messages"][0]["role"], "user");

        let gemini = encode_request(&request, ApiWireProtocol::GeminiGenerateContent)?;
        assert_eq!(gemini["systemInstruction"]["parts"][1]["text"], "secondary");
        assert_eq!(gemini["contents"].as_array().map(Vec::len), Some(1));
        assert_eq!(gemini["contents"][0]["role"], "user");
        Ok(())
    }

    #[tokio::test]
    async fn anthropic_stream_does_not_prefix_tool_arguments_with_empty_input() {
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg-1\",\"usage\":{\"input_tokens\":3}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call-1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":4}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::AnthropicMessages,
        )
        .collect::<Vec<_>>()
        .await;
        let response = events.iter().find_map(|event| match event {
            Ok(StreamEvent::Completed { response }) => Some(response),
            _ => None,
        });
        assert_eq!(
            response
                .and_then(|response| response.function_calls().ok())
                .and_then(|calls| calls.first().map(|call| call.arguments.clone()))
                .as_deref(),
            Some("{\"path\":\"src/lib.rs\"}")
        );
    }

    #[tokio::test]
    async fn malformed_chat_tool_call_is_not_silently_discarded() {
        let body = concat!(
            "data: {\"id\":\"chat-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        let events = parse_provider_stream(
            stream::once(async move { Ok(Bytes::from_static(body.as_bytes())) }),
            ApiWireProtocol::ChatCompletions,
        )
        .collect::<Vec<_>>()
        .await;
        let response = events.iter().find_map(|event| match event {
            Ok(StreamEvent::Completed { response }) => Some(response),
            _ => None,
        });
        assert!(response.is_some_and(|response| response.function_calls().is_err()));
    }
}
