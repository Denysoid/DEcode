use std::{
    collections::{BTreeMap, HashMap},
    hash::{Hash, Hasher},
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, timeout, timeout_at},
};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    LspConnectionState, LspError, LspServerConfig, LspShared, MAX_TEXT_FIELD_BYTES, sanitize_text,
};
use crate::notice::UiNotice;

const CHANNEL_CAPACITY: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const SHUTDOWN_RESPONSE_GRACE: Duration = Duration::from_millis(600);
const PROCESS_REAP_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub(crate) struct LspDocument {
    pub uri: String,
    pub language_id: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LspQuery {
    pub operation: String,
    method: &'static str,
    params: Value,
    document: Option<LspDocument>,
}

impl LspQuery {
    pub(crate) fn workspace_symbols(query: String) -> Self {
        Self {
            operation: "workspace_symbols".to_owned(),
            method: "workspace/symbol",
            params: json!({ "query": query }),
            document: None,
        }
    }

    pub(crate) fn document_symbols(document: LspDocument) -> Self {
        Self {
            operation: "document_symbols".to_owned(),
            method: "textDocument/documentSymbol",
            params: json!({ "textDocument": { "uri": document.uri } }),
            document: Some(document),
        }
    }

    pub(crate) fn definition(document: LspDocument, line: u64, column: u64) -> Self {
        Self::position(
            "definition",
            "textDocument/definition",
            document,
            line,
            column,
            None,
        )
    }

    pub(crate) fn references(
        document: LspDocument,
        line: u64,
        column: u64,
        include_declaration: bool,
    ) -> Self {
        Self::position(
            "references",
            "textDocument/references",
            document,
            line,
            column,
            Some(json!({ "includeDeclaration": include_declaration })),
        )
    }

    pub(crate) fn hover(document: LspDocument, line: u64, column: u64) -> Self {
        Self::position("hover", "textDocument/hover", document, line, column, None)
    }

    fn position(
        operation: &str,
        method: &'static str,
        document: LspDocument,
        line: u64,
        column: u64,
        context: Option<Value>,
    ) -> Self {
        let mut params = serde_json::Map::from_iter([
            ("textDocument".to_owned(), json!({ "uri": document.uri })),
            (
                "position".to_owned(),
                json!({
                    "line": line.saturating_sub(1),
                    "character": column.saturating_sub(1),
                }),
            ),
        ]);
        if let Some(context) = context {
            params.insert("context".to_owned(), context);
        }
        Self {
            operation: operation.to_owned(),
            method,
            params: Value::Object(params),
            document: Some(document),
        }
    }
}

enum ClientCommand {
    Query {
        id: u64,
        query: LspQuery,
        response: oneshot::Sender<Result<Value, LspError>>,
    },
    Cancel {
        id: u64,
    },
    Shutdown {
        acknowledged: oneshot::Sender<()>,
    },
}

pub(crate) struct LspClient {
    server: String,
    request_timeout: Duration,
    command_tx: mpsc::Sender<ClientCommand>,
    next_id: AtomicU64,
    task: JoinHandle<()>,
}

impl LspClient {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn connect(
        config: LspServerConfig,
        root: PathBuf,
        startup_timeout: Duration,
        request_timeout: Duration,
        max_message_bytes: usize,
        shared: Arc<LspShared>,
    ) -> Result<Self, LspError> {
        let server = config.name.clone();
        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PAGER", "cat")
            .env("GIT_PAGER", "cat")
            .env("NO_COLOR", "1");
        let mut child = command.spawn().map_err(|error| LspError::Startup {
            server: server.clone(),
            message: error.to_string(),
        })?;
        let Some(mut stdin) = child.stdin.take() else {
            kill_and_reap(&mut child).await;
            return Err(LspError::Startup {
                server,
                message: "child stdin was not piped".to_owned(),
            });
        };
        let Some(stdout) = child.stdout.take() else {
            kill_and_reap(&mut child).await;
            return Err(LspError::Startup {
                server,
                message: "child stdout was not piped".to_owned(),
            });
        };
        let stderr_task = child.stderr.take().map(|stderr| {
            let task_server = server.clone();
            tokio::spawn(async move {
                drain_stderr(stderr, &task_server).await;
            })
        });
        let (inbound_tx, mut inbound_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let reader_server = server.clone();
        let reader_task = tokio::spawn(async move {
            read_loop(stdout, max_message_bytes, &reader_server, inbound_tx).await;
        });

        let initialize = initialize_server(
            &server,
            &root,
            &mut stdin,
            &mut inbound_rx,
            max_message_bytes,
            &shared,
        );
        match timeout(startup_timeout, initialize).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                reader_task.abort();
                if let Some(task) = stderr_task {
                    task.abort();
                }
                kill_and_reap(&mut child).await;
                return Err(error);
            }
            Err(_) => {
                reader_task.abort();
                if let Some(task) = stderr_task {
                    task.abort();
                }
                kill_and_reap(&mut child).await;
                return Err(LspError::Timeout {
                    server,
                    operation: "initialize".to_owned(),
                    secs: startup_timeout.as_secs(),
                });
            }
        }

        let (command_tx, command_rx) = mpsc::channel(CHANNEL_CAPACITY);
        let actor_server = server.clone();
        let task = tokio::spawn(run_actor(ActorContext {
            server: actor_server,
            child,
            stdin,
            inbound_rx,
            command_rx,
            reader_task,
            stderr_task,
            shared,
            max_message_bytes,
        }));
        Ok(Self {
            server,
            request_timeout,
            command_tx,
            next_id: AtomicU64::new(2),
            task,
        })
    }

    pub(crate) async fn query(
        &self,
        query: LspQuery,
        cancel: &CancellationToken,
    ) -> Result<Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed).max(2);
        let (response_tx, response_rx) = oneshot::channel();
        let send = self.command_tx.send(ClientCommand::Query {
            id,
            query: query.clone(),
            response: response_tx,
        });
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(LspError::Cancelled { server: self.server.clone(), operation: query.operation }),
            result = send => result.map_err(|_| LspError::ChannelClosed { server: self.server.clone() })?,
        }
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                self.cancel_request(id);
                return Err(LspError::Cancelled { server: self.server.clone(), operation: query.operation });
            }
            result = timeout(self.request_timeout, response_rx) => result,
        };
        match response {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(LspError::ChannelClosed {
                server: self.server.clone(),
            }),
            Err(_) => {
                self.cancel_request(id);
                Err(LspError::Timeout {
                    server: self.server.clone(),
                    operation: query.operation,
                    secs: self.request_timeout.as_secs(),
                })
            }
        }
    }

    fn cancel_request(&self, id: u64) {
        let command = ClientCommand::Cancel { id };
        if let Err(mpsc::error::TrySendError::Full(command)) = self.command_tx.try_send(command) {
            let sender = self.command_tx.clone();
            drop(tokio::spawn(async move {
                let _ = sender.send(command).await;
            }));
        }
    }

    pub(crate) async fn shutdown(mut self, deadline: Duration) {
        let deadline = Instant::now() + deadline;
        let (ack_tx, ack_rx) = oneshot::channel();
        if matches!(
            timeout_at(
                deadline,
                self.command_tx.send(ClientCommand::Shutdown {
                    acknowledged: ack_tx,
                })
            )
            .await,
            Ok(Ok(()))
        ) {
            let _ = timeout_at(deadline, ack_rx).await;
        }
        if timeout_at(deadline, &mut self.task).await.is_err() {
            self.task.abort();
            let _ = self.task.await;
        }
    }
}

struct ActorContext {
    server: String,
    child: Child,
    stdin: ChildStdin,
    inbound_rx: mpsc::Receiver<Result<Value, LspError>>,
    command_rx: mpsc::Receiver<ClientCommand>,
    reader_task: JoinHandle<()>,
    stderr_task: Option<JoinHandle<()>>,
    shared: Arc<LspShared>,
    max_message_bytes: usize,
}

async fn run_actor(mut context: ActorContext) {
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, LspError>>> = HashMap::new();
    let mut opened_documents: BTreeMap<String, (i64, u64)> = BTreeMap::new();
    loop {
        tokio::select! {
            command = context.command_rx.recv() => {
                match command {
                    Some(ClientCommand::Query { id, query, response }) => {
                        let prepared = prepare_document(
                            &context.server,
                            &mut context.stdin,
                            &mut opened_documents,
                            query.document.as_ref(),
                            context.max_message_bytes,
                        ).await;
                        if let Err(error) = prepared {
                            let _ = response.send(Err(error));
                            continue;
                        }
                        let request = json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "method": query.method,
                            "params": query.params,
                        });
                        if let Err(error) = write_message(
                            &mut context.stdin,
                            &request,
                            context.max_message_bytes,
                            &context.server,
                        ).await {
                            let _ = response.send(Err(error));
                            continue;
                        }
                        pending.insert(id, response);
                    }
                    Some(ClientCommand::Cancel { id }) => {
                        pending.remove(&id);
                        let cancellation = json!({
                            "jsonrpc": "2.0",
                            "method": "$/cancelRequest",
                            "params": { "id": id },
                        });
                        let _ = write_message(
                            &mut context.stdin,
                            &cancellation,
                            context.max_message_bytes,
                            &context.server,
                        ).await;
                    }
                    Some(ClientCommand::Shutdown { acknowledged }) => {
                        graceful_protocol_shutdown(&mut context).await;
                        let _ = acknowledged.send(());
                        break;
                    }
                    None => {
                        graceful_protocol_shutdown(&mut context).await;
                        break;
                    }
                }
            }
            inbound = context.inbound_rx.recv() => {
                match inbound {
                    Some(Ok(message)) => {
                        if let Err(error) = handle_inbound(
                            &context.server,
                            message,
                            &mut context.stdin,
                            context.max_message_bytes,
                            &context.shared,
                            &mut pending,
                        ).await {
                            context.shared.set_state(
                                &context.server,
                                LspConnectionState::Error,
                                UiNotice::external(error.to_string()),
                            );
                        }
                    }
                    Some(Err(error)) => {
                        context.shared.set_state(
                            &context.server,
                            LspConnectionState::Error,
                            UiNotice::external(error.to_string()),
                        );
                        break;
                    }
                    None => {
                        context.shared.set_state(
                            &context.server,
                            LspConnectionState::Error,
                            UiNotice::Stopped,
                        );
                        break;
                    }
                }
            }
            status = context.child.wait() => {
                let detail = status.map_or_else(
                    |error| format!("process wait failed: {error}"),
                    |status| format!("language server exited with {status}"),
                );
                context.shared.set_state(
                    &context.server,
                    LspConnectionState::Error,
                    UiNotice::external(detail),
                );
                break;
            }
        }
    }
    for (_, response) in pending {
        let _ = response.send(Err(LspError::ChannelClosed {
            server: context.server.clone(),
        }));
    }
    context.reader_task.abort();
    if let Some(task) = context.stderr_task {
        task.abort();
    }
    kill_and_reap(&mut context.child).await;
}

async fn initialize_server(
    server: &str,
    root: &PathBuf,
    stdin: &mut ChildStdin,
    inbound_rx: &mut mpsc::Receiver<Result<Value, LspError>>,
    max_message_bytes: usize,
    shared: &Arc<LspShared>,
) -> Result<(), LspError> {
    let root_uri = Url::from_directory_path(root)
        .map_err(|()| LspError::Startup {
            server: server.to_owned(),
            message: "workspace root could not be encoded as a file URI".to_owned(),
        })?
        .to_string();
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "processId": null,
            "clientInfo": { "name": "decode", "version": env!("CARGO_PKG_VERSION") },
            "locale": "en",
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "symbol": { "dynamicRegistration": false }
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "didSave": true
                    },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                    "definition": { "linkSupport": true },
                    "references": { "dynamicRegistration": false },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "versionSupport": true
                    }
                },
                "window": { "workDoneProgress": true }
            },
            "initializationOptions": null,
            "trace": "off"
        }
    });
    write_message(stdin, &request, max_message_bytes, server).await?;
    loop {
        let message = inbound_rx.recv().await.ok_or_else(|| LspError::Startup {
            server: server.to_owned(),
            message: "language server closed stdout during initialization".to_owned(),
        })??;
        if message.get("id").and_then(Value::as_u64) == Some(1) {
            if let Some(error) = message.get("error") {
                return Err(LspError::Startup {
                    server: server.to_owned(),
                    message: sanitize_text(&error.to_string(), MAX_TEXT_FIELD_BYTES),
                });
            }
            break;
        }
        handle_preinitialized_message(server, message, stdin, max_message_bytes, shared).await?;
    }
    write_message(
        stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
        max_message_bytes,
        server,
    )
    .await
}

async fn handle_preinitialized_message(
    server: &str,
    message: Value,
    stdin: &mut ChildStdin,
    max_message_bytes: usize,
    shared: &Arc<LspShared>,
) -> Result<(), LspError> {
    if message.get("method").and_then(Value::as_str) == Some("textDocument/publishDiagnostics") {
        if let Some(params) = message.get("params") {
            shared.publish_diagnostics(server, params);
        }
        return Ok(());
    }
    if message.get("id").is_some() && message.get("method").is_some() {
        respond_to_server_request(server, &message, stdin, max_message_bytes).await?;
    }
    Ok(())
}

async fn handle_inbound(
    server: &str,
    message: Value,
    stdin: &mut ChildStdin,
    max_message_bytes: usize,
    shared: &Arc<LspShared>,
    pending: &mut HashMap<u64, oneshot::Sender<Result<Value, LspError>>>,
) -> Result<(), LspError> {
    if let Some(id) = message.get("id").and_then(Value::as_u64)
        && message.get("method").is_none()
    {
        if let Some(response) = pending.remove(&id) {
            let result = if let Some(error) = message.get("error") {
                Err(LspError::Protocol {
                    server: server.to_owned(),
                    message: sanitize_text(&error.to_string(), MAX_TEXT_FIELD_BYTES),
                })
            } else {
                Ok(message.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = response.send(result);
        }
        return Ok(());
    }
    let method = message.get("method").and_then(Value::as_str);
    if method == Some("textDocument/publishDiagnostics") {
        if let Some(params) = message.get("params") {
            shared.publish_diagnostics(server, params);
        }
    } else if message.get("id").is_some() && method.is_some() {
        respond_to_server_request(server, &message, stdin, max_message_bytes).await?;
    }
    Ok(())
}

async fn respond_to_server_request(
    server: &str,
    message: &Value,
    stdin: &mut ChildStdin,
    max_message_bytes: usize,
) -> Result<(), LspError> {
    let Some(id) = message.get("id").cloned() else {
        return Ok(());
    };
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let response = match method {
        "workspace/configuration" => {
            let count = message
                .pointer("/params/items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            json!({ "jsonrpc": "2.0", "id": id, "result": vec![Value::Null; count] })
        }
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability" => {
            json!({ "jsonrpc": "2.0", "id": id, "result": null })
        }
        "workspace/workspaceFolders" => {
            json!({ "jsonrpc": "2.0", "id": id, "result": null })
        }
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not supported by this read-only client" }
        }),
    };
    write_message(stdin, &response, max_message_bytes, server).await
}

async fn prepare_document(
    server: &str,
    stdin: &mut ChildStdin,
    opened: &mut BTreeMap<String, (i64, u64)>,
    document: Option<&LspDocument>,
    max_message_bytes: usize,
) -> Result<(), LspError> {
    let Some(document) = document else {
        return Ok(());
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    document.text.hash(&mut hasher);
    let hash = hasher.finish();
    match opened.get(&document.uri).copied() {
        None => {
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": document.uri,
                        "languageId": document.language_id,
                        "version": 1,
                        "text": document.text,
                    }
                }
            });
            write_message(stdin, &notification, max_message_bytes, server).await?;
            opened.insert(document.uri.clone(), (1, hash));
        }
        Some((version, previous_hash)) if previous_hash != hash => {
            let version = version.saturating_add(1);
            let notification = json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": document.uri, "version": version },
                    "contentChanges": [{ "text": document.text }]
                }
            });
            write_message(stdin, &notification, max_message_bytes, server).await?;
            opened.insert(document.uri.clone(), (version, hash));
        }
        Some(_) => {}
    }
    Ok(())
}

async fn graceful_protocol_shutdown(context: &mut ActorContext) {
    let shutdown_id = u64::MAX;
    let _ = write_message(
        &mut context.stdin,
        &json!({ "jsonrpc": "2.0", "id": shutdown_id, "method": "shutdown", "params": null }),
        context.max_message_bytes,
        &context.server,
    )
    .await;
    let wait_for_response = async {
        while let Some(message) = context.inbound_rx.recv().await {
            match message {
                Ok(message) if message.get("id").and_then(Value::as_u64) == Some(shutdown_id) => {
                    break;
                }
                Ok(message) => {
                    let _ = handle_preinitialized_message(
                        &context.server,
                        message,
                        &mut context.stdin,
                        context.max_message_bytes,
                        &context.shared,
                    )
                    .await;
                }
                Err(_) => break,
            }
        }
    };
    let _ = timeout(SHUTDOWN_RESPONSE_GRACE, wait_for_response).await;
    let _ = write_message(
        &mut context.stdin,
        &json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
        context.max_message_bytes,
        &context.server,
    )
    .await;
}

async fn write_message(
    writer: &mut ChildStdin,
    value: &Value,
    max_message_bytes: usize,
    server: &str,
) -> Result<(), LspError> {
    let body = serde_json::to_vec(value).map_err(|error| LspError::Protocol {
        server: server.to_owned(),
        message: format!("could not serialize client message: {error}"),
    })?;
    if body.len() > max_message_bytes {
        return Err(LspError::Protocol {
            server: server.to_owned(),
            message: format!(
                "outbound message is {} bytes, exceeding the {} byte limit",
                body.len(),
                max_message_bytes
            ),
        });
    }
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|error| LspError::Protocol {
            server: server.to_owned(),
            message: format!("failed to write LSP header: {error}"),
        })?;
    writer
        .write_all(&body)
        .await
        .map_err(|error| LspError::Protocol {
            server: server.to_owned(),
            message: format!("failed to write LSP body: {error}"),
        })?;
    writer.flush().await.map_err(|error| LspError::Protocol {
        server: server.to_owned(),
        message: format!("failed to flush LSP message: {error}"),
    })
}

async fn read_loop<R: AsyncRead + Unpin>(
    reader: R,
    max_message_bytes: usize,
    server: &str,
    sender: mpsc::Sender<Result<Value, LspError>>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        match read_message(&mut reader, max_message_bytes, server).await {
            Ok(Some(message)) => {
                if sender.send(Ok(message)).await.is_err() {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }
}

async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    max_message_bytes: usize,
    server: &str,
) -> Result<Option<Value>, LspError> {
    let mut content_length = None;
    let mut header_bytes = 0_usize;
    loop {
        let Some(line) = read_header_line(
            reader,
            MAX_HEADER_BYTES.saturating_sub(header_bytes),
            server,
        )
        .await?
        else {
            return if header_bytes == 0 {
                Ok(None)
            } else {
                Err(LspError::Protocol {
                    server: server.to_owned(),
                    message: "EOF in LSP header".to_owned(),
                })
            };
        };
        header_bytes = header_bytes.saturating_add(line.len());
        let line = std::str::from_utf8(&line).map_err(|_| LspError::Protocol {
            server: server.to_owned(),
            message: "LSP header is not valid UTF-8".to_owned(),
        })?;
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.trim().eq_ignore_ascii_case("Content-Length")
        {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| LspError::Protocol {
                    server: server.to_owned(),
                    message: "invalid Content-Length header".to_owned(),
                })?;
            if content_length.replace(parsed).is_some() {
                return Err(LspError::Protocol {
                    server: server.to_owned(),
                    message: "duplicate Content-Length header".to_owned(),
                });
            }
        }
    }
    let length = content_length.ok_or_else(|| LspError::Protocol {
        server: server.to_owned(),
        message: "missing Content-Length header".to_owned(),
    })?;
    if length == 0 || length > max_message_bytes {
        return Err(LspError::Protocol {
            server: server.to_owned(),
            message: format!(
                "LSP message length {length} is outside the allowed 1..={max_message_bytes} range"
            ),
        });
    }
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| LspError::Protocol {
            server: server.to_owned(),
            message: format!("failed to read LSP body: {error}"),
        })?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|error| LspError::Protocol {
            server: server.to_owned(),
            message: format!("invalid JSON-RPC payload: {error}"),
        })
}

async fn read_header_line<R: AsyncRead + Unpin>(
    reader: &mut BufReader<R>,
    remaining: usize,
    server: &str,
) -> Result<Option<Vec<u8>>, LspError> {
    let mut line = Vec::new();
    loop {
        let available = reader
            .fill_buf()
            .await
            .map_err(|error| LspError::Protocol {
                server: server.to_owned(),
                message: format!("failed to read LSP header: {error}"),
            })?;
        if available.is_empty() {
            return Ok((!line.is_empty()).then_some(line));
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index.saturating_add(1));
        if length > remaining.saturating_sub(line.len()) {
            return Err(LspError::Protocol {
                server: server.to_owned(),
                message: format!("LSP header exceeds {MAX_HEADER_BYTES} bytes"),
            });
        }
        let complete = available.get(length.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..length]);
        reader.consume(length);
        if complete {
            return Ok(Some(line));
        }
    }
}

async fn drain_stderr<R: AsyncRead + Unpin>(reader: R, server: &str) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let message = sanitize_text(&line, 2_048);
                tracing::debug!(server, bytes = message.len(), "LSP server wrote to stderr");
            }
        }
    }
}

async fn kill_and_reap(child: &mut Child) {
    if let Ok(Some(_)) = child.try_wait() {
        return;
    }
    let _ = child.start_kill();
    let _ = timeout(PROCESS_REAP_GRACE, child.wait()).await;
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{Arc, atomic::AtomicU64},
        time::Duration,
    };

    use tokio::{
        io::{AsyncWriteExt, BufReader},
        sync::{mpsc, oneshot},
        time::timeout,
    };
    use tokio_util::sync::CancellationToken;

    use super::{ClientCommand, LspClient, LspError, LspQuery, read_message};

    #[tokio::test]
    async fn framing_accepts_crlf_and_preserves_json() -> Result<(), Box<dyn std::error::Error>> {
        let body = br#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        let mut reader = BufReader::new(Cursor::new(bytes));
        let value = read_message(&mut reader, 4096, "test")
            .await?
            .ok_or("missing message")?;
        assert_eq!(value["id"], 7);
        assert_eq!(value["result"]["ok"], true);
        Ok(())
    }

    #[tokio::test]
    async fn framing_rejects_duplicate_or_oversized_lengths() {
        let duplicate = b"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        let mut reader = BufReader::new(Cursor::new(duplicate));
        assert!(read_message(&mut reader, 4096, "test").await.is_err());

        let oversized = b"Content-Length: 5000\r\n\r\n".to_vec();
        let mut reader = BufReader::new(Cursor::new(oversized));
        assert!(read_message(&mut reader, 4096, "test").await.is_err());
    }

    #[tokio::test]
    async fn framing_rejects_an_unterminated_oversized_header_without_waiting_for_eof() {
        let (mut writer, reader) = tokio::io::duplex(1_024);
        let writer_task = tokio::spawn(async move {
            for _ in 0..20 {
                if writer.write_all(&[b'a'; 1_024]).await.is_err() {
                    return;
                }
            }
            std::future::pending::<()>().await;
        });
        let mut reader = BufReader::new(reader);

        let result = timeout(
            Duration::from_millis(200),
            read_message(&mut reader, 4_096, "test"),
        )
        .await;
        writer_task.abort();
        assert!(matches!(result, Ok(Err(_))));
    }

    #[tokio::test]
    async fn shutdown_is_bounded_when_the_actor_channel_is_full() {
        let (command_tx, _command_rx) = mpsc::channel(1);
        assert!(
            command_tx
                .send(ClientCommand::Cancel { id: 1 })
                .await
                .is_ok()
        );
        let client = LspClient {
            server: "test".to_owned(),
            request_timeout: Duration::from_secs(1),
            command_tx,
            next_id: AtomicU64::new(2),
            task: tokio::spawn(std::future::pending()),
        };

        assert!(
            timeout(
                Duration::from_millis(200),
                client.shutdown(Duration::from_millis(20))
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn cancellation_is_not_lost_when_the_actor_channel_is_full() {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let client = Arc::new(LspClient {
            server: "test".to_owned(),
            request_timeout: Duration::from_secs(1),
            command_tx,
            next_id: AtomicU64::new(2),
            task: tokio::spawn(std::future::pending()),
        });
        let (seen_tx, seen_rx) = oneshot::channel();
        let (drain_tx, drain_rx) = oneshot::channel();
        let actor = tokio::spawn(async move {
            let Some(ClientCommand::Query { id, response, .. }) = command_rx.recv().await else {
                return None;
            };
            let _response = response;
            let _ = seen_tx.send(id);
            let _ = drain_rx.await;
            let _filler = command_rx.recv().await;
            timeout(Duration::from_millis(200), command_rx.recv())
                .await
                .ok()
                .flatten()
        });
        let cancel = CancellationToken::new();
        let query_client = Arc::clone(&client);
        let query_cancel = cancel.clone();
        let query = tokio::spawn(async move {
            query_client
                .query(
                    LspQuery::workspace_symbols("needle".to_owned()),
                    &query_cancel,
                )
                .await
        });
        let id = seen_rx.await.unwrap_or_default();
        assert!(
            client
                .command_tx
                .send(ClientCommand::Cancel { id: 999 })
                .await
                .is_ok()
        );
        cancel.cancel();
        assert!(matches!(query.await, Ok(Err(LspError::Cancelled { .. }))));
        let _ = drain_tx.send(());
        assert!(matches!(
            actor.await,
            Ok(Some(ClientCommand::Cancel { id: cancelled })) if cancelled == id
        ));
        client.task.abort();
    }
}
