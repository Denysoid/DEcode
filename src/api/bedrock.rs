use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_stream::stream;
use aws_config::{BehaviorVersion, Region};
use aws_credential_types::provider::SharedCredentialsProvider;
use aws_sdk_bedrockruntime::{
    Client,
    types::{
        ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, DocumentBlock,
        DocumentFormat, DocumentSource, ImageBlock, ImageFormat, ImageSource,
        InferenceConfiguration, Message, SystemContentBlock, Tool, ToolConfiguration,
        ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolSpecification, ToolUseBlock,
    },
};
use aws_smithy_types::{Blob, Document, Number, retry::RetryConfig};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::{
    api::types::{
        InputItem, InputMessage, InputTokenDetails, OutputItem, ResponseError, ResponseStatus,
        ResponsesRequest, ResponsesResponse, Role, StreamEvent, UsageStats,
    },
    config::ApiConfig,
    error::ApiError,
};

static RESPONSE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_BEDROCK_ERROR_CHARS: usize = 2_048;

/// Native AWS Bedrock Runtime transport. The client is initialized lazily so
/// constructing DEcode never probes IMDS or the credential chain until this
/// provider is actually selected for a request.
#[derive(Clone)]
pub(crate) struct BedrockRuntimeTransport {
    config: ApiConfig,
    client: Arc<OnceCell<Client>>,
}

impl BedrockRuntimeTransport {
    pub(crate) fn new(config: ApiConfig) -> Self {
        Self {
            config,
            client: Arc::new(OnceCell::new()),
        }
    }

    pub(crate) async fn stream_response_attempt(
        &self,
        request: ResponsesRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ApiError>>, ApiError> {
        let client = self.client(&cancel).await?.clone();
        let prepared = prepare_request(&request)?;
        let operation = client
            .converse_stream()
            .model_id(&request.model)
            .set_messages(Some(prepared.messages))
            .set_system(Some(prepared.system))
            .inference_config(
                InferenceConfiguration::builder()
                    .max_tokens(i32::try_from(request.max_output_tokens).unwrap_or(i32::MAX))
                    .set_temperature(request.temperature)
                    .build(),
            )
            .set_tool_config(prepared.tools);

        let send = operation.send();
        let mut output = tokio::select! {
            _ = cancel.cancelled() => return Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.config.request_timeout, send) => {
                match result {
                    Ok(Ok(output)) => output,
                    Ok(Err(error)) => return Err(bedrock_error("request", &error.to_string())),
                    Err(_) => return Err(ApiError::RequestTimeout {
                        secs: self.config.request_timeout.as_secs(),
                    }),
                }
            }
        };

        let idle_timeout = self.config.stream_idle_timeout;
        let response_id = next_response_id();
        let event_stream = stream! {
            yield Ok(StreamEvent::Created {
                response: response_shell(&response_id, None, Vec::new(), None),
            });

            let mut text = String::new();
            let mut tool_calls = BTreeMap::<i32, PendingToolCall>::new();
            let mut usage = None;
            let mut stop_reason = None;
            loop {
                let received = tokio::select! {
                    _ = cancel.cancelled() => {
                        yield Err(ApiError::Cancelled);
                        return;
                    }
                    result = tokio::time::timeout(idle_timeout, output.stream.recv()) => result,
                };
                let event = match received {
                    Ok(Ok(Some(event))) => event,
                    Ok(Ok(None)) => break,
                    Ok(Err(error)) => {
                        yield Err(bedrock_error("stream", &error.to_string()));
                        return;
                    }
                    Err(_) => {
                        yield Err(ApiError::IdleTimeout { secs: idle_timeout.as_secs() });
                        return;
                    }
                };

                match event {
                    aws_sdk_bedrockruntime::types::ConverseStreamOutput::ContentBlockDelta(event) => {
                        match event.delta() {
                            Some(ContentBlockDelta::Text(delta)) => {
                                text.push_str(delta);
                                yield Ok(StreamEvent::OutputTextDelta { delta: delta.clone() });
                            }
                            Some(ContentBlockDelta::ToolUse(delta)) => {
                                if let Some(call) = tool_calls.get_mut(&event.content_block_index()) {
                                    call.arguments.push_str(delta.input());
                                }
                            }
                            _ => {}
                        }
                    }
                    aws_sdk_bedrockruntime::types::ConverseStreamOutput::ContentBlockStart(event) => {
                        if let Some(ContentBlockStart::ToolUse(start)) = event.start() {
                            tool_calls.insert(event.content_block_index(), PendingToolCall {
                                call_id: start.tool_use_id().to_owned(),
                                name: start.name().to_owned(),
                                arguments: String::new(),
                            });
                        }
                    }
                    aws_sdk_bedrockruntime::types::ConverseStreamOutput::Metadata(event) => {
                        usage = event.usage().map(usage_from_bedrock);
                    }
                    aws_sdk_bedrockruntime::types::ConverseStreamOutput::MessageStop(event) => {
                        stop_reason = Some(event.stop_reason().as_str().to_owned());
                    }
                    aws_sdk_bedrockruntime::types::ConverseStreamOutput::ContentBlockStop(_)
                    | aws_sdk_bedrockruntime::types::ConverseStreamOutput::MessageStart(_) => {}
                    _ => {}
                }
            }

            if !text.is_empty() {
                yield Ok(StreamEvent::OutputTextDone { text: text.clone() });
            }
            yield terminal_stream_event(
                &response_id,
                &text,
                tool_calls.into_values(),
                usage,
                stop_reason.as_deref(),
            );
        };
        Ok(Box::pin(event_stream))
    }

    async fn client(&self, cancel: &CancellationToken) -> Result<&Client, ApiError> {
        let config = self.config.clone();
        tokio::select! {
            _ = cancel.cancelled() => Err(ApiError::Cancelled),
            result = tokio::time::timeout(self.config.request_timeout, self.client.get_or_try_init(|| async move {
                build_client(&config).await
            })) => match result {
                Ok(result) => result,
                Err(_) => Err(ApiError::RequestTimeout { secs: self.config.request_timeout.as_secs() }),
            },
        }
    }
}

async fn build_client(config: &ApiConfig) -> Result<Client, ApiError> {
    let mut loader =
        aws_config::defaults(BehaviorVersion::latest()).retry_config(RetryConfig::disabled());
    if let Some(region) = &config.bedrock_runtime.region {
        loader = loader.region(Region::new(region.clone()));
    }
    if let Some(profile) = &config.bedrock_runtime.profile {
        loader = loader.profile_name(profile);
    }
    let mut shared = loader.load().await;
    if let Some(role_arn) = &config.bedrock_runtime.role_arn {
        let provider = aws_config::sts::AssumeRoleProvider::builder(role_arn)
            .configure(&shared)
            .session_name("decode-bedrock")
            .build()
            .await;
        shared = shared
            .to_builder()
            .credentials_provider(SharedCredentialsProvider::new(provider))
            .build();
    }
    let mut service = aws_sdk_bedrockruntime::config::Builder::from(&shared)
        .retry_config(RetryConfig::disabled());
    if let Some(endpoint) = &config.bedrock_runtime.endpoint_url {
        service = service.endpoint_url(endpoint);
    }
    Ok(Client::from_conf(service.build()))
}

struct PreparedRequest {
    messages: Vec<Message>,
    system: Vec<SystemContentBlock>,
    tools: Option<ToolConfiguration>,
}

fn prepare_request(request: &ResponsesRequest) -> Result<PreparedRequest, ApiError> {
    let mut system = vec![SystemContentBlock::Text(request.instructions.clone())];
    let mut messages = Vec::new();
    for item in request.input.iter() {
        match item {
            InputItem::Message(message) => match message.role {
                Role::System | Role::Developer => {
                    system.push(SystemContentBlock::Text(message.content.clone()));
                }
                Role::User | Role::Assistant => {
                    push_message(&mut messages, message_to_bedrock(message)?);
                }
            },
            InputItem::FunctionCallOutput(output) => {
                let block = ToolResultBlock::builder()
                    .tool_use_id(&output.call_id)
                    .content(ToolResultContentBlock::Text(output.output.clone()))
                    .build()
                    .map_err(build_error)?;
                push_message(
                    &mut messages,
                    Message::builder()
                        .role(ConversationRole::User)
                        .content(ContentBlock::ToolResult(block))
                        .build()
                        .map_err(build_error)?,
                );
            }
            InputItem::Opaque(value) => push_opaque_item(&mut messages, value)?,
        }
    }
    if messages.is_empty() {
        return Err(ApiError::Protocol(
            "Bedrock Runtime request contains no user or assistant messages".to_owned(),
        ));
    }

    let tools = request
        .tools
        .as_deref()
        .map(tool_configuration)
        .transpose()?;
    Ok(PreparedRequest {
        messages,
        system,
        tools,
    })
}

fn message_to_bedrock(message: &InputMessage) -> Result<Message, ApiError> {
    let role = match message.role {
        Role::User => ConversationRole::User,
        Role::Assistant => ConversationRole::Assistant,
        Role::System | Role::Developer => {
            return Err(ApiError::Protocol(
                "system/developer messages must be translated as Bedrock system content".to_owned(),
            ));
        }
    };
    Message::builder()
        .role(role)
        .content(ContentBlock::Text(message.content.clone()))
        .build()
        .map_err(build_error)
}

fn push_opaque_item(messages: &mut Vec<Message>, value: &Value) -> Result<(), ApiError> {
    match value.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            let call_id = required(value, "call_id")?;
            let name = required(value, "name")?;
            let arguments = required(value, "arguments")?;
            let arguments: Value = serde_json::from_str(arguments).map_err(|error| {
                ApiError::Protocol(format!(
                    "Bedrock tool arguments are not valid JSON: {error}"
                ))
            })?;
            let block = ToolUseBlock::builder()
                .tool_use_id(call_id)
                .name(name)
                .input(json_to_document(&arguments)?)
                .build()
                .map_err(build_error)?;
            push_message(
                messages,
                Message::builder()
                    .role(ConversationRole::Assistant)
                    .content(ContentBlock::ToolUse(block))
                    .build()
                    .map_err(build_error)?,
            );
            Ok(())
        }
        Some("message") | None if value.get("role").is_some() => {
            let role = match required(value, "role")? {
                "user" => ConversationRole::User,
                "assistant" => ConversationRole::Assistant,
                "system" | "developer" => {
                    return Err(ApiError::Protocol(
                        "opaque system items cannot appear in Bedrock message history".to_owned(),
                    ));
                }
                other => {
                    return Err(ApiError::Protocol(format!(
                        "Bedrock cannot replay unknown message role {other:?}"
                    )));
                }
            };
            let content = opaque_content(value)?;
            push_message(
                messages,
                Message::builder()
                    .role(role)
                    .set_content(Some(content))
                    .build()
                    .map_err(build_error)?,
            );
            Ok(())
        }
        other => Err(ApiError::Protocol(format!(
            "Bedrock Runtime cannot losslessly replay Responses item type {other:?}; start or fork a native Bedrock session"
        ))),
    }
}

fn opaque_content(value: &Value) -> Result<Vec<ContentBlock>, ApiError> {
    let Some(content) = value.get("content") else {
        return Err(ApiError::Protocol(
            "Bedrock message item is missing content".to_owned(),
        ));
    };
    if let Some(text) = content.as_str() {
        return Ok(vec![ContentBlock::Text(text.to_owned())]);
    }
    let parts = content.as_array().ok_or_else(|| {
        ApiError::Protocol("Bedrock message content must be text or an array".to_owned())
    })?;
    let mut blocks = Vec::with_capacity(parts.len());
    for part in parts {
        let kind = part.get("type").and_then(Value::as_str);
        match kind {
            Some("input_text") | Some("output_text") | Some("text") => {
                blocks.push(ContentBlock::Text(required(part, "text")?.to_owned()));
            }
            Some("input_image") => blocks.push(image_content(part)?),
            Some("input_file") => blocks.push(document_content(part)?),
            other => {
                return Err(ApiError::Protocol(format!(
                    "Bedrock native conversion does not support content part {other:?} yet"
                )));
            }
        }
    }
    Ok(blocks)
}

fn image_content(part: &Value) -> Result<ContentBlock, ApiError> {
    let (media_type, bytes) = decode_data_url(required(part, "image_url")?)?;
    let format = match media_type {
        "image/gif" => ImageFormat::Gif,
        "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
        "image/png" => ImageFormat::Png,
        "image/webp" => ImageFormat::Webp,
        other => {
            return Err(ApiError::Protocol(format!(
                "Bedrock Runtime does not support image media type {other:?}"
            )));
        }
    };
    let image = ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(build_error)?;
    Ok(ContentBlock::Image(image))
}

fn document_content(part: &Value) -> Result<ContentBlock, ApiError> {
    let (media_type, bytes) = decode_data_url(required(part, "file_data")?)?;
    let filename = part
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("attachment.txt");
    let format = document_format(media_type, filename)?;
    // Bedrock warns that document names are prompt-injection surfaces. Keep a
    // neutral, deterministic label instead of replaying the user filename.
    let document = DocumentBlock::builder()
        .format(format)
        .name("DEcode attachment")
        .source(DocumentSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(build_error)?;
    Ok(ContentBlock::Document(document))
}

fn document_format(media_type: &str, filename: &str) -> Result<DocumentFormat, ApiError> {
    let extension = filename
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    let format = match (media_type, extension.as_deref()) {
        ("text/csv", _) | (_, Some("csv")) => DocumentFormat::Csv,
        ("application/msword", _) | (_, Some("doc")) => DocumentFormat::Doc,
        ("application/vnd.openxmlformats-officedocument.wordprocessingml.document", _)
        | (_, Some("docx")) => DocumentFormat::Docx,
        ("text/html", _) | (_, Some("html" | "htm")) => DocumentFormat::Html,
        ("text/markdown", _) | (_, Some("md" | "markdown")) => DocumentFormat::Md,
        ("application/pdf", _) | (_, Some("pdf")) => DocumentFormat::Pdf,
        ("text/plain", _) | (_, Some("txt")) => DocumentFormat::Txt,
        ("application/vnd.ms-excel", _) | (_, Some("xls")) => DocumentFormat::Xls,
        ("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", _)
        | (_, Some("xlsx")) => DocumentFormat::Xlsx,
        _ => {
            return Err(ApiError::Protocol(format!(
                "Bedrock Runtime does not support document media type {media_type:?}"
            )));
        }
    };
    Ok(format)
}

fn decode_data_url(value: &str) -> Result<(&str, Vec<u8>), ApiError> {
    let value = value
        .strip_prefix("data:")
        .ok_or_else(|| ApiError::Protocol("Bedrock attachment is not a data URL".to_owned()))?;
    let (metadata, encoded) = value
        .split_once(',')
        .ok_or_else(|| ApiError::Protocol("Bedrock attachment data URL is malformed".to_owned()))?;
    let media_type = metadata.strip_suffix(";base64").ok_or_else(|| {
        ApiError::Protocol("Bedrock attachment data URL is not base64".to_owned())
    })?;
    let bytes = STANDARD.decode(encoded).map_err(|error| {
        ApiError::Protocol(format!("Bedrock attachment base64 is malformed: {error}"))
    })?;
    Ok((media_type, bytes))
}

fn push_message(messages: &mut Vec<Message>, message: Message) {
    if let Some(previous) = messages.last_mut()
        && previous.role == message.role
    {
        previous.content.extend(message.content);
    } else {
        messages.push(message);
    }
}

fn tool_configuration(
    definitions: &[crate::api::types::FunctionToolDefinition],
) -> Result<ToolConfiguration, ApiError> {
    let mut tools = Vec::with_capacity(definitions.len());
    for definition in definitions {
        let spec = ToolSpecification::builder()
            .name(&definition.name)
            .set_description(definition.description.clone())
            .input_schema(ToolInputSchema::Json(json_to_document(
                &definition.parameters,
            )?))
            .strict(true)
            .build()
            .map_err(build_error)?;
        tools.push(Tool::ToolSpec(spec));
    }
    ToolConfiguration::builder()
        .set_tools(Some(tools))
        .build()
        .map_err(build_error)
}

fn json_to_document(value: &Value) -> Result<Document, ApiError> {
    match value {
        Value::Null => Ok(Document::Null),
        Value::Bool(value) => Ok(Document::Bool(*value)),
        Value::String(value) => Ok(Document::String(value.clone())),
        Value::Array(values) => values
            .iter()
            .map(json_to_document)
            .collect::<Result<Vec<_>, _>>()
            .map(Document::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), json_to_document(value)?)))
            .collect::<Result<HashMap<_, _>, ApiError>>()
            .map(Document::Object),
        Value::Number(value) => {
            let number = if let Some(value) = value.as_u64() {
                Number::PosInt(value)
            } else if let Some(value) = value.as_i64() {
                Number::NegInt(value)
            } else if let Some(value) = value.as_f64() {
                if !value.is_finite() {
                    return Err(ApiError::Protocol(
                        "Bedrock JSON contains a non-finite number".to_owned(),
                    ));
                }
                Number::Float(value)
            } else {
                return Err(ApiError::Protocol(
                    "Bedrock JSON contains an unsupported number".to_owned(),
                ));
            };
            Ok(Document::Number(number))
        }
    }
}

#[derive(Debug)]
struct PendingToolCall {
    call_id: String,
    name: String,
    arguments: String,
}

fn response_outputs(
    text: &str,
    tool_calls: impl Iterator<Item = PendingToolCall>,
) -> Vec<OutputItem> {
    let mut output = Vec::new();
    if !text.is_empty() {
        output.push(OutputItem(json!({
            "type": "message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}],
        })));
    }
    output.extend(tool_calls.map(|call| {
        OutputItem(json!({
            "type": "function_call",
            "call_id": call.call_id,
            "name": call.name,
            "arguments": call.arguments,
        }))
    }));
    output
}

fn terminal_stream_event(
    id: &str,
    text: &str,
    tool_calls: impl Iterator<Item = PendingToolCall>,
    usage: Option<UsageStats>,
    stop_reason: Option<&str>,
) -> Result<StreamEvent, ApiError> {
    let stop_reason = stop_reason.ok_or_else(|| {
        ApiError::Protocol("Bedrock stream ended before the message stop event".to_owned())
    })?;
    let output = response_outputs(text, tool_calls);
    let (status, error) = match stop_reason {
        "end_turn" | "stop_sequence" | "tool_use" => (ResponseStatus::Completed, None),
        "max_tokens" | "model_context_window_exceeded" => (
            ResponseStatus::Incomplete,
            Some(ResponseError {
                code: Some(format!("bedrock_{stop_reason}")),
                message: format!("Bedrock stopped before completing the response: {stop_reason}"),
                param: None,
            }),
        ),
        "content_filtered"
        | "guardrail_intervened"
        | "malformed_model_output"
        | "malformed_tool_use" => (
            ResponseStatus::Failed,
            Some(ResponseError {
                code: Some(format!("bedrock_{stop_reason}")),
                message: format!("Bedrock rejected the response: {stop_reason}"),
                param: None,
            }),
        ),
        _ => (
            ResponseStatus::Incomplete,
            Some(ResponseError {
                code: Some("bedrock_unknown_stop_reason".to_owned()),
                message: "Bedrock returned an unrecognized stop reason".to_owned(),
                param: None,
            }),
        ),
    };
    let mut response = response_shell(id, Some(status.clone()), output, usage);
    response.error = error;
    Ok(match status {
        ResponseStatus::Completed => StreamEvent::Completed { response },
        ResponseStatus::Failed => StreamEvent::Failed { response },
        ResponseStatus::Incomplete => StreamEvent::Incomplete { response },
        ResponseStatus::Cancelled | ResponseStatus::Unknown => unreachable!(),
    })
}

fn response_shell(
    id: &str,
    status: Option<ResponseStatus>,
    output: Vec<OutputItem>,
    usage: Option<UsageStats>,
) -> ResponsesResponse {
    ResponsesResponse {
        id: id.to_owned(),
        status,
        output,
        usage,
        created_at: Some(chrono::Utc::now().timestamp()),
        error: None,
    }
}

fn usage_from_bedrock(usage: &aws_sdk_bedrockruntime::types::TokenUsage) -> UsageStats {
    let input_tokens = u64::try_from(usage.input_tokens()).unwrap_or(0);
    let output_tokens = u64::try_from(usage.output_tokens()).unwrap_or(0);
    let total_tokens = u64::try_from(usage.total_tokens())
        .unwrap_or_else(|_| input_tokens.saturating_add(output_tokens));
    UsageStats {
        input_tokens,
        output_tokens,
        total_tokens,
        input_tokens_details: Some(InputTokenDetails {
            cached_tokens: usage
                .cache_read_input_tokens()
                .and_then(|value| u64::try_from(value).ok())
                .unwrap_or(0),
        }),
    }
}

fn next_response_id() -> String {
    let sequence = RESPONSE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "bedrock-{}-{sequence}",
        chrono::Utc::now().timestamp_millis()
    )
}

fn required<'a>(value: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::Protocol(format!("Bedrock item is missing {field:?}")))
}

fn build_error(error: impl std::fmt::Display) -> ApiError {
    ApiError::Protocol(format!("invalid Bedrock request: {error}"))
}

fn bedrock_error(stage: &str, raw: &str) -> ApiError {
    let message = raw
        .chars()
        .filter(|character| !character.is_control() || *character == '\n')
        .take(MAX_BEDROCK_ERROR_CHARS)
        .collect::<String>();
    let normalized = message.to_ascii_lowercase();
    let retryable = [
        "throttl",
        "timeout",
        "temporar",
        "service unavailable",
        "internalserver",
        "modelnotready",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    ApiError::Bedrock {
        stage: stage.to_owned(),
        message,
        retryable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{FunctionToolDefinition, ReasoningEffort};

    #[test]
    fn translates_messages_and_tools_without_credentials() -> Result<(), ApiError> {
        let request = ResponsesRequest::new(
            "anthropic.claude",
            "system",
            vec![InputMessage::user("hello")],
            512,
        )
        .with_reasoning(ReasoningEffort::High)
        .with_tools(vec![FunctionToolDefinition::new(
            "read_file",
            Some("read".to_owned()),
            json!({"type":"object","properties":{"path":{"type":"string"}}}),
        )]);
        let prepared = prepare_request(&request)?;
        assert_eq!(prepared.messages.len(), 1);
        assert_eq!(prepared.system.len(), 1);
        assert_eq!(
            prepared.tools.as_ref().map(|tools| tools.tools().len()),
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn opaque_function_call_round_trips_to_canonical_output() -> Result<(), ApiError> {
        let mut messages = Vec::new();
        push_opaque_item(
            &mut messages,
            &json!({
                "type":"function_call",
                "call_id":"call-1",
                "name":"read_file",
                "arguments":"{\"path\":\"src/lib.rs\"}"
            }),
        )?;
        assert_eq!(messages.len(), 1);
        let output = response_outputs(
            "done",
            [PendingToolCall {
                call_id: "call-2".to_owned(),
                name: "search_code".to_owned(),
                arguments: "{\"pattern\":\"x\"}".to_owned(),
            }]
            .into_iter(),
        );
        let response = response_shell(
            "bedrock-test",
            Some(ResponseStatus::Completed),
            output,
            None,
        );
        assert_eq!(response.output_text(), "done");
        assert_eq!(response.function_calls()?.len(), 1);
        Ok(())
    }

    #[test]
    fn translates_hydrated_image_and_document_parts() -> Result<(), ApiError> {
        let request = ResponsesRequest::stateless_replay(
            "anthropic.claude",
            "system",
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type":"input_text", "text":"inspect these"},
                    {"type":"input_image", "image_url":"data:image/png;base64,iVBORw0KGgo="},
                    {
                        "type":"input_file",
                        "filename":"notes.md",
                        "file_data":"data:text/markdown;base64,IyBOb3Rlcw=="
                    }
                ]
            })],
            512,
        );
        let prepared = prepare_request(&request)?;
        let content = &prepared.messages[0].content;
        assert!(matches!(content[0], ContentBlock::Text(_)));
        assert!(matches!(content[1], ContentBlock::Image(_)));
        assert!(matches!(content[2], ContentBlock::Document(_)));
        if let ContentBlock::Document(document) = &content[2] {
            assert_eq!(document.name(), "DEcode attachment");
            assert_eq!(document.format(), &DocumentFormat::Md);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_attachment_encoding_and_unsupported_media() {
        assert!(decode_data_url("data:image/png,not-base64").is_err());
        assert!(
            image_content(&json!({
                "image_url":"data:image/svg+xml;base64,PHN2Zz4="
            }))
            .is_err()
        );
        assert!(
            document_content(&json!({
                "filename":"archive.zip",
                "file_data":"data:application/zip;base64,UEs="
            }))
            .is_err()
        );
    }

    #[test]
    fn classifies_throttling_as_retryable_but_credentials_as_permanent() {
        assert!(matches!(
            bedrock_error("request", "ThrottlingException: slow down"),
            ApiError::Bedrock {
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            bedrock_error("credentials", "Credentials not loaded"),
            ApiError::Bedrock {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn maps_bedrock_token_limit_to_incomplete_response() -> Result<(), ApiError> {
        let event = terminal_stream_event(
            "bedrock-test",
            "partial",
            std::iter::empty(),
            None,
            Some("max_tokens"),
        )?;
        let StreamEvent::Incomplete { response } = event else {
            return Err(ApiError::Protocol(
                "token limit completed a response".to_owned(),
            ));
        };
        assert_eq!(response.status, Some(ResponseStatus::Incomplete));
        assert_eq!(
            response
                .error
                .as_ref()
                .and_then(|error| error.code.as_deref()),
            Some("bedrock_max_tokens")
        );
        Ok(())
    }

    #[test]
    fn maps_bedrock_filtering_to_failed_response() -> Result<(), ApiError> {
        let event = terminal_stream_event(
            "bedrock-test",
            "",
            std::iter::empty(),
            None,
            Some("content_filtered"),
        )?;
        let StreamEvent::Failed { response } = event else {
            return Err(ApiError::Protocol(
                "filtered output did not fail the response".to_owned(),
            ));
        };
        assert_eq!(response.status, Some(ResponseStatus::Failed));
        assert_eq!(
            response
                .error
                .as_ref()
                .and_then(|error| error.code.as_deref()),
            Some("bedrock_content_filtered")
        );
        Ok(())
    }

    #[test]
    fn rejects_bedrock_stream_without_message_stop() {
        assert!(matches!(
            terminal_stream_event(
                "bedrock-test",
                "partial",
                std::iter::empty(),
                None,
                None,
            ),
            Err(ApiError::Protocol(message))
                if message.contains("message stop")
        ));
    }
}
