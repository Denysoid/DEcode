use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use bytes::Bytes;
use decode::{
    agent::{
        AgentPhase, CommandScope, FollowUpMode, FollowUpStatus, HistoryKind, HistoryStatus,
        Orchestrator, OrchestratorCommand, OrchestratorEvent, PlanDecision, ReviewVerdict,
        ShellApprovalDecision, SideExchangeStatus, ToolResultStatus, UiModal, UiSnapshot, WhipKind,
    },
    api::{
        InputMessage, MAX_SSE_TURN_BYTES, ReasoningEffort, ResponsesClient, ResponsesRequest,
        parse_sse_stream,
    },
    attachments::AttachmentSource,
    config::{
        AgentConfig, ApiAuth, ApiConfig, ApiProvider, ContextMode, ProjectInstructionsConfig,
        ResponsesEndpoint, ShellConfig, SkillsConfig, SubagentConfig, WhipConfig,
    },
    error::ApiError,
    parser::ToolOutcome,
};
use futures_util::{StreamExt, stream};
use pretty_assertions::assert_eq;
use secrecy::SecretString;
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path},
};

fn api_config(server: &MockServer) -> ApiConfig {
    ApiConfig {
        provider: ApiProvider::Azure,
        auth: ApiAuth::ApiKey,
        api_key: SecretString::new("test-secret".into()),
        bedrock_runtime: decode::config::BedrockRuntimeConfig::default(),
        transport: decode::config::ApiTransport::Sse,
        endpoint: ResponsesEndpoint::FullUrl(format!("{}/responses?feature=on", server.uri())),
        allow_insecure_loopback: true,
        deployment: "test-deployment".to_owned(),
        deployment_choices: vec!["test-deployment".to_owned()],
        api_version: Some("2026-01-01-preview".to_owned()),
        max_output_tokens: 1_024,
        reasoning_effort: ReasoningEffort::Medium,
        temperature: None,
        server_compaction_threshold: None,
        request_timeout: Duration::from_secs(2),
        stream_idle_timeout: Duration::from_secs(2),
        max_attempts: 5,
        retry_min_delay: Duration::from_millis(1),
        retry_max_delay: Duration::from_millis(1),
        retry_after_cap: Duration::from_secs(120),
        pricing: decode::usage::PricingCatalog::default(),
        pricing_catalog_url: None,
    }
}

fn request() -> ResponsesRequest {
    ResponsesRequest::new(
        "test-deployment",
        "system instructions\n",
        vec![InputMessage::user("hello")],
        1_024,
    )
    .with_reasoning(ReasoningEffort::Medium)
}

fn completed_sse(text: &str) -> String {
    let escaped = serde_json::to_string(text).unwrap();
    format!(
        concat!(
            "data: {{\"type\":\"response.created\",\"response\":",
            "{{\"id\":\"r1\",\"status\":\"in_progress\",\"created_at\":123}}}}\r\n\r\n",
            "data:{{\"type\":\"response.output_text.delta\",\"delta\":{escaped}}}\n\n",
            ": heartbeat\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":",
            "{{\"id\":\"r1\",\"status\":\"completed\",\"created_at\":124,",
            "\"output\":[{{\"type\":\"message\",\"id\":\"m1\",\"role\":\"assistant\",",
            "\"content\":[{{\"type\":\"output_text\",\"text\":{escaped}}}]}}],",
            "\"usage\":{{\"input_tokens\":10,\"output_tokens\":5,\"total_tokens\":15}}}}}}\n\n",
            "data: [DONE]\n\n"
        ),
        escaped = escaped,
    )
}

fn completed_sse_with_usage(text: &str, input_tokens: u64, output_tokens: u64) -> String {
    let escaped = serde_json::to_string(text).unwrap();
    let total_tokens = input_tokens.saturating_add(output_tokens);
    format!(
        concat!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":{escaped}}}\n\n",
            "data: {{\"type\":\"response.completed\",\"response\":",
            "{{\"id\":\"usage-response\",\"status\":\"completed\",\"created_at\":124,",
            "\"output\":[{{\"type\":\"message\",\"id\":\"usage-message\",",
            "\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":{escaped}}}]}}],",
            "\"usage\":{{\"input_tokens\":{input_tokens},\"output_tokens\":{output_tokens},",
            "\"total_tokens\":{total_tokens}}}}}}}\n\ndata: [DONE]\n\n"
        ),
        escaped = escaped,
        input_tokens = input_tokens,
        output_tokens = output_tokens,
        total_tokens = total_tokens,
    )
}

fn azure_no_capacity_sse() -> String {
    concat!(
        "data:{\"type\":\"response.created\",\"response\":",
        "{\"id\":\"capacity\",\"status\":\"in_progress\",\"created_at\":123}}\n\n",
        "data:{\"type\":\"error\",\"error\":{",
        "\"type\":\"too_many_requests\",\"code\":\"no_capacity\",",
        "\"message\":\"peak demand\",\"param\":null},\"sequence_number\":2}\n\n",
        "data:{\"type\":\"response.failed\",\"response\":{",
        "\"id\":\"capacity\",\"status\":\"failed\",\"created_at\":123,",
        "\"error\":{\"code\":\"no_capacity\",\"message\":\"peak demand\"}}}\n\n"
    )
    .to_owned()
}

fn completed_sse_with_items(
    response_id: &str,
    text: &str,
    mut output: Vec<serde_json::Value>,
) -> String {
    if output.is_empty() {
        output.push(serde_json::json!({
            "type": "message",
            "id": format!("message-{response_id}"),
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": text}]
        }));
    }
    let delta = serde_json::json!({
        "type": "response.output_text.delta",
        "delta": text,
    });
    let completed = serde_json::json!({
        "type": "response.completed",
        "response": {
            "id": response_id,
            "status": "completed",
            "created_at": 124,
            "output": output,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15
            }
        }
    });
    format!("data: {delta}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn completed_sse_with_status(text: &str, status: Option<&str>) -> String {
    let mut response = serde_json::json!({
        "id": "terminal-status-test",
        "created_at": 124,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "total_tokens": 15
        },
        "output": [{
            "type": "message",
            "id": "terminal-message",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text}]
        }]
    });
    if let Some(status) = status {
        response["status"] = serde_json::Value::String(status.to_owned());
    }
    let event = serde_json::json!({
        "type": "response.completed",
        "response": response,
    });
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

fn incomplete_sse(response_id: &str, text: &str) -> String {
    let event = serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": response_id,
            "status": "incomplete",
            "created_at": 124,
            "output": [{
                "type": "message",
                "id": format!("message-{response_id}"),
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}]
            }]
        }
    });
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

fn agent_config(workspace: &Path, max_tool_iterations: u32) -> AgentConfig {
    let instructions_file = workspace.join("instructions.md");
    std::fs::write(
        &instructions_file,
        "Use the tagged tool protocol exactly.\n",
    )
    .unwrap();
    AgentConfig {
        context_mode: ContextMode::Stateless,
        context_budget: 32_000,
        max_context_budget: 2_000_000,
        max_tool_iterations,
        workspace_root: std::fs::canonicalize(workspace).unwrap(),
        session_dir: workspace.join(".test-sessions"),
        privacy_user_rules_file: workspace.join(".test-privacy.ignore"),
        instructions_file: std::fs::canonicalize(instructions_file).unwrap(),
        instructions: "Use the tagged tool protocol exactly.\n".to_owned(),
        project_instructions: ProjectInstructionsConfig::default(),
        skills: SkillsConfig {
            enabled: false,
            ..SkillsConfig::default()
        },
        exec_timeout: Duration::from_secs(2),
        subagents: SubagentConfig {
            enabled: false,
            allow_mcp: false,
            worktree_dir: workspace.join(".test-worktrees"),
            max_parallel: 1,
            max_per_session: 1,
            max_tool_iterations: 1,
            max_tokens_per_agent: 150_000,
            max_total_tokens_per_session: 500_000,
            max_depth: 3,
            max_children_per_agent: 4,
            task_timeout: Duration::from_secs(2),
            git_timeout: Duration::from_secs(2),
        },
        shell: ShellConfig::default(),
        whip: WhipConfig::default(),
    }
}

async fn start_orchestrator(
    api: ApiConfig,
    agent: AgentConfig,
) -> (
    TestCommandSender,
    mpsc::Receiver<OrchestratorEvent>,
    tokio::task::JoinHandle<()>,
) {
    let (event_tx, event_rx) = mpsc::channel(256);
    let (command_tx, command_rx) = mpsc::channel(64);
    let (orchestrator, mut snapshots) =
        Orchestrator::with_snapshot(api, agent, event_tx, command_rx).unwrap();
    let task = tokio::spawn(orchestrator.run());
    wait_for_initial_scope(&mut snapshots).await;
    (
        TestCommandSender::new(command_tx, snapshots),
        event_rx,
        task,
    )
}

async fn start_orchestrator_with_snapshot(
    api: ApiConfig,
    agent: AgentConfig,
    diagnostic_capacity: usize,
) -> (
    TestCommandSender,
    mpsc::Receiver<OrchestratorEvent>,
    watch::Receiver<UiSnapshot>,
    tokio::task::JoinHandle<()>,
) {
    let (event_tx, event_rx) = mpsc::channel(diagnostic_capacity);
    let (command_tx, command_rx) = mpsc::channel(64);
    let (orchestrator, snapshots) =
        Orchestrator::with_snapshot(api, agent, event_tx, command_rx).unwrap();
    let task = tokio::spawn(orchestrator.run());
    let mut command_snapshots = snapshots.clone();
    wait_for_initial_scope(&mut command_snapshots).await;
    (
        TestCommandSender::new(command_tx, command_snapshots),
        event_rx,
        snapshots,
        task,
    )
}

#[derive(Clone)]
struct TestCommandSender {
    inner: mpsc::Sender<OrchestratorCommand>,
    snapshots: watch::Receiver<UiSnapshot>,
}

type TestSendError = Box<mpsc::error::SendError<OrchestratorCommand>>;

impl TestCommandSender {
    fn new(
        inner: mpsc::Sender<OrchestratorCommand>,
        snapshots: watch::Receiver<UiSnapshot>,
    ) -> Self {
        Self { inner, snapshots }
    }

    async fn send(&self, command: OrchestratorCommand) -> Result<(), TestSendError> {
        self.inner.send(command).await.map_err(Box::new)
    }

    async fn submit(&self, prompt: impl Into<String>) -> Result<(), TestSendError> {
        let scope = {
            let snapshot = self.snapshots.borrow();
            CommandScope {
                conversation_epoch: snapshot.conversation_epoch,
                phase_revision: snapshot.phase_revision,
            }
        };
        self.send(OrchestratorCommand::Submit {
            prompt: prompt.into(),
            attachments: Vec::new(),
            scope,
        })
        .await
    }

    async fn submit_with_attachments(
        &self,
        prompt: impl Into<String>,
        attachments: Vec<AttachmentSource>,
    ) -> Result<(), TestSendError> {
        let scope = {
            let snapshot = self.snapshots.borrow();
            CommandScope {
                conversation_epoch: snapshot.conversation_epoch,
                phase_revision: snapshot.phase_revision,
            }
        };
        self.send(OrchestratorCommand::Submit {
            prompt: prompt.into(),
            attachments,
            scope,
        })
        .await
    }
}

async fn wait_for_initial_scope(snapshots: &mut watch::Receiver<UiSnapshot>) {
    while snapshots.borrow().phase_revision == 0 {
        snapshots.changed().await.unwrap();
    }
}

async fn wait_for_snapshot(
    snapshots: &mut watch::Receiver<UiSnapshot>,
    predicate: impl Fn(&UiSnapshot) -> bool,
) -> UiSnapshot {
    wait_for_snapshot_with_timeout(snapshots, Duration::from_secs(10), predicate).await
}

async fn wait_for_snapshot_with_timeout(
    snapshots: &mut watch::Receiver<UiSnapshot>,
    deadline: Duration,
    predicate: impl Fn(&UiSnapshot) -> bool,
) -> UiSnapshot {
    let result = tokio::time::timeout(deadline, async {
        loop {
            let current = snapshots.borrow().clone();
            if predicate(&current) {
                return current;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await;
    match result {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let current = snapshots.borrow();
            panic!(
                "snapshot update timed out: phase={:?}, history={}, assistant_bytes={}, status={}",
                current.phase,
                current.history.len(),
                current.assistant.len(),
                current.status
            );
        }
    }
}

async fn next_orchestrator_event(
    events: &mut mpsc::Receiver<OrchestratorEvent>,
) -> OrchestratorEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("orchestrator event timed out")
        .expect("orchestrator event channel closed")
}

#[derive(Clone)]
struct SequencedSse {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Vec<String>>,
    first_delay: Duration,
}

#[derive(Clone)]
struct SelectivelyDelayedSse {
    calls: Arc<AtomicUsize>,
    bodies: Arc<Vec<String>>,
    delayed_indices: Arc<Vec<usize>>,
    delay: Duration,
}

#[derive(Clone)]
struct AttachmentTokenCount {
    calls: Arc<AtomicUsize>,
    with_old_history: u64,
    compacted: u64,
}

#[derive(Clone)]
struct PdfOnlyTokenCount;

impl Respond for PdfOnlyTokenCount {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let contains_spreadsheet = body.to_string().to_ascii_lowercase().contains("sheet.xlsx");
        if contains_spreadsheet {
            return ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": {"message": "input token counting currently supports PDF files only"}
            }));
        }
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "response.input_tokens",
            "input_tokens": 100,
        }))
    }
}

impl Respond for AttachmentTokenCount {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let contains_old_history = body["input"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item.to_string().contains("old-response"))
        });
        let input_tokens = if contains_old_history {
            self.with_old_history
        } else {
            self.compacted
        };
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "response.input_tokens",
            "input_tokens": input_tokens,
        }))
    }
}

impl Respond for SelectivelyDelayedSse {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(index)
            .or_else(|| self.bodies.last())
            .cloned()
            .unwrap_or_else(|| completed_sse("done"));
        let response = ResponseTemplate::new(200).set_body_raw(body, "text/event-stream");
        if self.delayed_indices.contains(&index) {
            response.set_delay(self.delay)
        } else {
            response
        }
    }
}

impl Respond for SequencedSse {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(index)
            .or_else(|| self.bodies.last())
            .cloned()
            .unwrap_or_else(|| completed_sse("done"));
        let response = ResponseTemplate::new(200).set_body_raw(body, "text/event-stream");
        if index == 0 && !self.first_delay.is_zero() {
            response.set_delay(self.first_delay)
        } else {
            response
        }
    }
}

#[tokio::test]
async fn client_sends_official_wire_shape_and_collects_completed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_sse("Hello 👋"),
            "text/event-stream; charset=utf-8",
        ))
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let completed = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.response.id, "r1");
    assert_eq!(completed.text, "Hello 👋");
    assert_eq!(completed.response.usage.unwrap().total_tokens, 15);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    assert_eq!(sent.headers.get("api-key").unwrap(), "test-secret");
    assert_eq!(sent.headers.get("accept").unwrap(), "text/event-stream");
    let query: Vec<_> = sent.url.query_pairs().collect();
    assert!(
        query
            .iter()
            .any(|pair| pair == &("feature".into(), "on".into()))
    );
    assert!(
        query
            .iter()
            .any(|pair| { pair == &("api-version".into(), "2026-01-01-preview".into()) })
    );

    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "test-deployment");
    assert_eq!(body["instructions"], "system instructions\n");
    assert_eq!(body["input"][0]["role"], "user");
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
}

#[tokio::test]
async fn client_counts_exact_input_tokens_on_the_responses_subresource() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "object": "response.input_tokens",
            "input_tokens": 12_345,
        })))
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let counted = client
        .count_input_tokens(&request(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(counted, Some(12_345));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/responses/input_tokens");
    assert_eq!(
        requests[0].url.query(),
        Some("feature=on&api-version=2026-01-01-preview")
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "test-deployment");
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn client_counts_non_pdf_file_input_without_sending_it_to_the_pdf_only_counter() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(PdfOnlyTokenCount)
        .mount(&server)
        .await;

    let file_bytes = b"spreadsheet payload that must remain intact";
    let pdf_bytes = b"pdf payload";
    let request = ResponsesRequest::stateless_replay(
        "test-deployment",
        "system instructions\n",
        vec![serde_json::json!({
            "type": "message",
            "role": "user",
            "content": [
                {
                    "type": "input_file",
                    "filename": "sheet.xlsx",
                    "file_data": format!(
                        "data:application/vnd.openxmlformats-officedocument.spreadsheetml.sheet;base64,{}",
                        STANDARD.encode(file_bytes)
                    )
                },
                {
                    "type": "input_file",
                    "filename": "report.pdf",
                    "file_data": format!(
                        "data:application/pdf;base64,{}",
                        STANDARD.encode(pdf_bytes)
                    )
                },
                {"type": "input_text", "text": "Inspect the spreadsheet"}
            ]
        })],
        1_024,
    );
    let client = ResponsesClient::new(api_config(&server)).unwrap();

    let counted = client
        .count_input_tokens(&request, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();

    assert!(counted > 100);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let serialized = body.to_string();
    assert!(!serialized.contains("sheet.xlsx"));
    assert!(!serialized.contains(&STANDARD.encode(file_bytes)));
    assert!(serialized.contains("counted locally"));
    assert!(serialized.contains("report.pdf"));
    assert!(serialized.contains(&STANDARD.encode(pdf_bytes)));
    let original = serde_json::to_string(&request).unwrap();
    assert!(original.contains("sheet.xlsx"));
    assert!(original.contains(&STANDARD.encode(file_bytes)));
    assert!(original.contains("report.pdf"));
}

#[tokio::test]
async fn non_pdf_attachment_is_only_substituted_in_token_preflight() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(PdfOnlyTokenCount)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("spreadsheet-complete"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let file_bytes = b"small spreadsheet payload";
    std::fs::write(workspace.path().join("sheet.xlsx"), file_bytes).unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 8_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;

    commands
        .submit_with_attachments(
            "inspect this spreadsheet",
            vec![AttachmentSource::Workspace("sheet.xlsx".to_owned())],
        )
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "spreadsheet-complete"
    })
    .await;

    let requests = server.received_requests().await.unwrap();
    let response_request = requests
        .iter()
        .find(|request| request.url.path() == "/responses")
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&response_request.body).unwrap();
    let serialized = body.to_string();
    assert!(serialized.contains("sheet.xlsx"));
    assert!(serialized.contains(&STANDARD.encode(file_bytes)));

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn attachment_preflight_compacts_old_history_before_the_model_request() {
    let server = MockServer::start().await;
    let response_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: response_calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_usage("old-response", 20_000, 100),
                completed_sse("attachment-complete"),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;
    let count_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(AttachmentTokenCount {
            calls: count_calls.clone(),
            with_old_history: 7_500,
            compacted: 2_000,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("screen.png"), b"bounded image bytes").unwrap();
    std::fs::write(
        workspace.path().join("sheet.xlsx"),
        b"bounded spreadsheet bytes",
    )
    .unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 8_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;

    commands.submit("first anchor").await.unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "old-response"
    })
    .await;
    commands
        .submit_with_attachments(
            "inspect this image",
            vec![
                AttachmentSource::Workspace("screen.png".to_owned()),
                AttachmentSource::Workspace("sheet.xlsx".to_owned()),
            ],
        )
        .await
        .unwrap();
    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "attachment-complete"
    })
    .await;

    assert_eq!(response_calls.load(Ordering::SeqCst), 2);
    assert!(count_calls.load(Ordering::SeqCst) >= 2);
    assert!(completed.history.iter().any(|entry| {
        entry
            .content
            .contains("older history entries compacted into deterministic API-context summaries")
    }));

    let requests = server.received_requests().await.unwrap();
    let response_bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/responses")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(response_bodies.len(), 2);
    let second_input = response_bodies[1]["input"].as_array().unwrap();
    assert!(
        !second_input
            .iter()
            .any(|item| item.to_string().contains("old-response"))
    );
    assert!(
        second_input
            .iter()
            .any(|item| item.to_string().contains("first anchor"))
    );
    assert!(
        second_input
            .iter()
            .any(|item| item.to_string().contains("inspect this image"))
    );
    assert!(second_input.iter().any(|item| {
        item.to_string()
            .contains("data:image/png;base64,Ym91bmRlZCBpbWFnZSBieXRlcw==")
    }));
    assert!(
        second_input
            .iter()
            .any(|item| item.to_string().contains("sheet.xlsx"))
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn attachment_preflight_rejects_only_when_the_newest_input_itself_is_too_large() {
    let server = MockServer::start().await;
    let response_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: response_calls.clone(),
            bodies: Arc::new(vec![completed_sse("must not be sent")]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;
    let count_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(AttachmentTokenCount {
            calls: count_calls.clone(),
            with_old_history: 9_000,
            compacted: 9_000,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("oversized.pdf"), b"bounded pdf bytes").unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 8_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;
    commands
        .submit_with_attachments(
            "inspect this document",
            vec![AttachmentSource::Workspace("oversized.pdf".to_owned())],
        )
        .await
        .unwrap();

    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    let AgentPhase::Error { message, .. } = failed.phase else {
        unreachable!();
    };
    assert!(message.contains("current prompt/attachments require 9000 input tokens"));
    assert!(message.contains("current attachments are never truncated"));
    assert_eq!(count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(response_calls.load(Ordering::SeqCst), 0);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn attachment_request_uses_safe_local_fallback_when_counter_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("fallback-complete"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("fallback.png"),
        b"fallback image bytes",
    )
    .unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 2),
        64,
    )
    .await;
    commands
        .submit_with_attachments(
            "inspect this image",
            vec![AttachmentSource::Workspace("fallback.png".to_owned())],
        )
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "fallback-complete"
    })
    .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/responses/input_tokens")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/responses")
            .count(),
        1
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn attachment_request_falls_back_when_azure_rejects_token_counting_for_the_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "This model is not supported by Responses API.",
                "type": "invalid_request_error"
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("fallback-complete"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(
        workspace.path().join("fallback.png"),
        b"fallback image bytes",
    )
    .unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 2),
        64,
    )
    .await;
    commands
        .submit_with_attachments(
            "inspect this image",
            vec![AttachmentSource::Workspace("fallback.png".to_owned())],
        )
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "fallback-complete"
    })
    .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/responses/input_tokens")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/responses")
            .count(),
        1
    );
    let response_request = requests
        .iter()
        .find(|request| request.url.path() == "/responses")
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&response_request.body).unwrap();
    let serialized = body.to_string();
    assert!(serialized.contains("input_image"));
    assert!(
        serialized.contains(&STANDARD.encode(b"fallback image bytes")),
        "fallback request omitted the attached image: {serialized}"
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn token_counting_preserves_unrelated_bad_request_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "message": "Malformed input item.",
                "type": "invalid_request_error"
            }
        })))
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = client
        .count_input_tokens(&request(), &CancellationToken::new())
        .await
        .unwrap_err();

    assert!(matches!(error, ApiError::Http { status: 400, .. }));
}

#[tokio::test]
async fn plan_mode_compacts_attachment_context_before_its_read_only_pass() {
    let server = MockServer::start().await;
    let response_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: response_calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_usage("old-response", 20_000, 100),
                completed_sse("Inspect the attachment, then report the verified result."),
                completed_sse("plan-complete"),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;
    let count_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(AttachmentTokenCount {
            calls: count_calls.clone(),
            with_old_history: 7_500,
            compacted: 2_000,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("plan.png"), b"plan image bytes").unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 8_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;
    commands.submit("first anchor").await.unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "old-response"
    })
    .await;

    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::SetPlanMode {
            enabled: true,
            scope,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| snapshot.work_modes.plan).await;
    commands
        .submit_with_attachments(
            "plan from this image",
            vec![AttachmentSource::Workspace("plan.png".to_owned())],
        )
        .await
        .unwrap();
    let pending = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.modal, Some(UiModal::PlanApproval { .. }))
    })
    .await;
    let UiModal::PlanApproval { review } = pending.modal.unwrap() else {
        unreachable!();
    };
    commands
        .send(OrchestratorCommand::DecidePlan {
            turn_id: review.turn_id,
            review_id: review.review_id,
            decision: PlanDecision::Approve {
                plan: review.plan.clone(),
            },
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "plan-complete"
    })
    .await;

    assert_eq!(response_calls.load(Ordering::SeqCst), 3);
    assert!(count_calls.load(Ordering::SeqCst) >= 2);
    let requests = server.received_requests().await.unwrap();
    let response_bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/responses")
        .map(|request| serde_json::from_slice::<serde_json::Value>(&request.body).unwrap())
        .collect::<Vec<_>>();
    assert!(
        !response_bodies[1]["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.to_string().contains("old-response"))
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn openai_provider_uses_bearer_auth_and_never_leaks_azure_header() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("openai-ok"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut config = api_config(&server);
    config.provider = ApiProvider::OpenAi;
    config.auth = ApiAuth::Bearer;
    config.api_version = None;
    let client = ResponsesClient::new(config).unwrap();
    let completed = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.text, "openai-ok");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer test-secret"
    );
    assert!(requests[0].headers.get("api-key").is_none());
    assert!(
        requests[0]
            .url
            .query_pairs()
            .all(|(name, _)| name != "api-version")
    );
}

#[tokio::test]
async fn compatible_provider_honours_explicit_api_key_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("compatible-ok"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let mut config = api_config(&server);
    config.provider = ApiProvider::Compatible;
    config.auth = ApiAuth::ApiKey;
    config.api_version = None;
    let client = ResponsesClient::new(config).unwrap();
    let completed = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.text, "compatible-ok");

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers.get("api-key").unwrap(), "test-secret");
    assert!(requests[0].headers.get("authorization").is_none());
}

#[tokio::test]
async fn google_provider_translates_chat_completions_and_streams_back_canonical_text() {
    let server = MockServer::start().await;
    let gemini_sse = concat!(
        "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gemini-ok\"}]}}]}\n\n",
        "data: {\"candidates\":[{\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2,\"totalTokenCount\":6}}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1beta/models/test-deployment:streamGenerateContent"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(gemini_sse, "text/event-stream"))
        .mount(&server)
        .await;

    let mut config = api_config(&server);
    config.provider = ApiProvider::Google;
    config.auth = ApiAuth::GoogleKey;
    config.endpoint = ResponsesEndpoint::FullUrl(format!("{}/v1beta/models", server.uri()));
    config.api_version = None;
    let completed = ResponsesClient::new(config)
        .unwrap()
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.text, "gemini-ok");
    assert_eq!(completed.response.usage.unwrap().total_tokens, 6);

    let requests = server.received_requests().await.unwrap();
    let sent = &requests[0];
    assert_eq!(sent.headers.get("x-goog-api-key").unwrap(), "test-secret");
    assert!(sent.headers.get("api-key").is_none());
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "system instructions\n"
    );
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 1_024);
    assert!(body.get("input").is_none());
    assert!(
        sent.url
            .query_pairs()
            .any(|pair| pair == ("alt".into(), "sse".into()))
    );
}

#[tokio::test]
async fn anthropic_provider_uses_native_headers_body_and_canonical_stream() {
    let server = MockServer::start().await;
    let anthropic_sse = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"claude-1\",\"usage\":{\"input_tokens\":7}}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"claude-ok\"}}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(anthropic_sse, "text/event-stream"))
        .mount(&server)
        .await;

    let mut config = api_config(&server);
    config.provider = ApiProvider::Anthropic;
    config.auth = ApiAuth::AnthropicKey;
    config.endpoint = ResponsesEndpoint::FullUrl(format!("{}/messages", server.uri()));
    config.api_version = None;
    let completed = ResponsesClient::new(config)
        .unwrap()
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.text, "claude-ok");
    assert_eq!(completed.response.usage.unwrap().total_tokens, 10);

    let requests = server.received_requests().await.unwrap();
    let sent = &requests[0];
    assert_eq!(sent.headers.get("x-api-key").unwrap(), "test-secret");
    assert_eq!(sent.headers.get("anthropic-version").unwrap(), "2023-06-01");
    assert!(sent.headers.get("authorization").is_none());
    let body: serde_json::Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["system"], "system instructions\n");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["max_tokens"], 1_024);
}

#[tokio::test]
async fn openai_provider_resolves_the_official_responses_endpoint() {
    let server = MockServer::start().await;
    let mut config = api_config(&server);
    config.provider = ApiProvider::OpenAi;
    config.auth = ApiAuth::Bearer;
    config.endpoint = ResponsesEndpoint::OpenAi;
    config.api_version = None;
    config.allow_insecure_loopback = false;

    let client = ResponsesClient::new(config).unwrap();
    assert_eq!(
        client.responses_url().as_str(),
        "https://api.openai.com/v1/responses"
    );
}

#[derive(Clone)]
struct FailThenSucceed {
    calls: Arc<AtomicUsize>,
    failure_status: u16,
    retry_after: Option<&'static str>,
}

impl Respond for FailThenSucceed {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let mut response = ResponseTemplate::new(self.failure_status).set_body_string("retry");
            if let Some(retry_after) = self.retry_after {
                response = response.insert_header("retry-after", retry_after);
            }
            response
        } else {
            ResponseTemplate::new(200).set_body_raw(completed_sse("ok"), "text/event-stream")
        }
    }
}

#[tokio::test]
async fn retries_429_and_honours_numeric_retry_after() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(FailThenSucceed {
            calls: calls.clone(),
            failure_status: 429,
            retry_after: Some("0"),
        })
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let response = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn accepts_http_date_retry_after_on_5xx() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(FailThenSucceed {
            calls: calls.clone(),
            failure_status: 503,
            // A past HTTP-date is a valid zero-delay retry instruction.
            retry_after: Some("Wed, 21 Oct 2015 07:28:00 GMT"),
        })
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let response = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(response.text, "ok");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unsupported_server_compaction_is_reported_without_fallback_duplication() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "error": {
                "code": "unsupported_parameter",
                "message": "context_management is not supported by this deployment"
            }
        })))
        .mount(&server)
        .await;

    let mut request = request();
    request.context_management = Some(vec![serde_json::json!({
        "type": "compaction",
        "compact_threshold": 50_000
    })]);
    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = client
        .completed_response(request, CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::Http { status: 400, ref body, .. }
            if body.contains("context_management is not supported")
    ));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        1,
        "client must not retry without compaction"
    );
    let sent: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["context_management"][0]["type"], "compaction");
    assert_eq!(sent["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn retries_5xx() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(FailThenSucceed {
            calls: calls.clone(),
            failure_status: 503,
            retry_after: None,
        })
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retries_nested_azure_no_capacity_then_succeeds() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![azure_no_capacity_sse(), completed_sse("recovered")]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let completed = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(completed.text, "recovered");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn nested_azure_no_capacity_stops_at_configured_retry_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(azure_no_capacity_sse(), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::RetryExhausted { attempts: 5, ref last_error }
            if last_error.contains("no_capacity") && last_error.contains("peak demand")
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn retry_exhaustion_performs_exactly_five_attempts() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(503).set_body_string("still unavailable"))
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ApiError::RetryExhausted { attempts: 5, .. }
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 5);
}

#[tokio::test]
async fn cancellation_interrupts_retry_backoff() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(FailThenSucceed {
            calls: calls.clone(),
            failure_status: 503,
            retry_after: Some("60"),
        })
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let cancellation = CancellationToken::new();
    let operation = tokio::spawn({
        let cancellation = cancellation.clone();
        async move { client.completed_response(request(), cancellation).await }
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cancellation.cancel();

    let error = tokio::time::timeout(Duration::from_secs(2), operation)
        .await
        .expect("cancelled backoff did not wake")
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, ApiError::Cancelled));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn single_attempt_primitive_never_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(503).set_body_string("try later"))
        .mount(&server)
        .await;

    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = match client
        .stream_response_attempt(request(), CancellationToken::new())
        .await
    {
        Ok(_) => panic!("503 unexpectedly opened an SSE stream"),
        Err(error) => error,
    };
    assert!(matches!(error, ApiError::Http { status: 503, .. }));
    assert!(client.is_retryable(&error));
    assert_eq!(client.max_attempts(), 5);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn never_retries_401_or_403() {
    for status in [401, 403] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(status).set_body_string("denied"))
            .mount(&server)
            .await;
        let client = ResponsesClient::new(api_config(&server)).unwrap();
        let error = client
            .completed_response(request(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Http { status: actual, .. } if actual == status));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn never_follows_http_redirects() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/target"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/target"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("wrong host path"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let error = ResponsesClient::new(api_config(&server))
        .unwrap()
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Http { status: 307, .. }));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url.path(), "/responses");
}

#[tokio::test]
async fn rejects_wrong_mime_and_premature_eof() {
    let mime_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("{}", "application/json"))
        .mount(&mime_server)
        .await;
    let error = ResponsesClient::new(api_config(&mime_server))
        .unwrap()
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::InvalidContentType { .. }));

    let eof_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data:{\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "text/event-stream",
        ))
        .mount(&eof_server)
        .await;
    let error = ResponsesClient::new(api_config(&eof_server))
        .unwrap()
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(error, ApiError::Protocol(message) if message.contains("without response.completed"))
    );
}

#[tokio::test]
async fn malformed_sse_and_pre_cancel_are_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("data: not-json\n\n", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let client = ResponsesClient::new(api_config(&server)).unwrap();
    let error = client
        .completed_response(request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Protocol(_)));

    let token = CancellationToken::new();
    token.cancel();
    let error = client
        .completed_response(request(), token)
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Cancelled));
}

#[test]
fn stateful_request_only_contains_unsent_input_and_repeats_instructions() {
    let request = ResponsesRequest::stateful(
        "deployment",
        "always repeat me",
        vec![InputMessage::tool_result(
            7,
            "read_file",
            "success",
            "contents",
        )],
        512,
        "resp_previous",
    );
    let value = serde_json::to_value(request).unwrap();
    assert_eq!(value["instructions"], "always repeat me");
    assert_eq!(value["previous_response_id"], "resp_previous");
    assert_eq!(value["store"], true);
    assert_eq!(value["input"].as_array().unwrap().len(), 1);
    assert_eq!(value["input"][0]["role"], "user");
    let envelope: serde_json::Value =
        serde_json::from_str(value["input"][0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(envelope["action_id"], 7);
    assert_eq!(envelope["tool"], "read_file");
}

#[tokio::test]
async fn orchestrator_never_executes_tools_from_premature_eof() {
    let server = MockServer::start().await;
    let partial = concat!(
        "<write_file><path>must-not-exist.txt</path>",
        "<content>unsafe partial</content></write_file>"
    );
    let body = format!(
        "data:{{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n",
        serde_json::to_string(partial).unwrap()
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 2)).await;
    commands.submit("write a file").await.unwrap();

    let mut saw_protocol_error = false;
    for _ in 0..16 {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::RecoverableError { message, .. } => {
                saw_protocol_error = message.contains("before response.completed");
                break;
            }
            OrchestratorEvent::ToolStarted { .. } => {
                panic!("partial tool tag was executed")
            }
            _ => {}
        }
    }

    assert!(saw_protocol_error);
    assert!(!workspace.path().join("must-not-exist.txt").exists());
    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "a partial Azure stream must stop at the manual recovery boundary instead of reconnecting automatically"
    );
    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn orchestrator_binds_confirmation_and_continues_after_decline() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let command_turn = concat!(
        "<execute_command>",
        "<command>echo should-not-run</command>",
        "<requires_confirmation>false</requires_confirmation>",
        "</execute_command>"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse(command_turn),
                completed_sse("Command was declined; continuing safely."),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 3)).await;
    commands.submit("run a command").await.unwrap();

    let action_id = loop {
        if let OrchestratorEvent::ConfirmationRequested {
            turn_id,
            action_id,
            model_requested,
            ..
        } = next_orchestrator_event(&mut events).await
        {
            assert_eq!(turn_id, 1);
            assert!(!model_requested);
            break action_id;
        }
    };

    commands
        .send(OrchestratorCommand::Confirm {
            turn_id: 1,
            action_id: action_id.saturating_add(1),
            decision: ShellApprovalDecision::RunOnce,
        })
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "stale confirmation unexpectedly changed the state"
    );

    commands
        .send(OrchestratorCommand::Confirm {
            turn_id: 1,
            action_id,
            decision: ShellApprovalDecision::Decline,
        })
        .await
        .unwrap();

    let mut saw_decline = false;
    let mut saw_done = false;
    while !saw_done {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::ToolCompleted { outcome, .. } => {
                saw_decline = matches!(outcome, ToolOutcome::Declined { .. });
            }
            OrchestratorEvent::Done { turn_id } => {
                assert_eq!(turn_id, 1);
                saw_done = true;
            }
            _ => {}
        }
    }
    assert!(saw_decline);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn exact_session_grant_skips_only_local_policy_prompts() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let locally_guarded = concat!(
        "<execute_command>",
        "<command>echo permission-probe</command>",
        "<requires_confirmation>false</requires_confirmation>",
        "</execute_command>"
    );
    let model_guarded = concat!(
        "<execute_command>",
        "<command>echo permission-probe</command>",
        "<requires_confirmation>true</requires_confirmation>",
        "</execute_command>"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse(locally_guarded),
                completed_sse(locally_guarded),
                completed_sse(model_guarded),
                completed_sse("Permission boundaries were preserved."),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 6)).await;
    commands
        .submit("exercise exact session trust")
        .await
        .unwrap();

    let mut confirmations = 0_usize;
    let mut successful_commands = 0_usize;
    let mut declined_commands = 0_usize;
    let mut saw_done = false;
    while !saw_done {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::ConfirmationRequested {
                turn_id,
                action_id,
                model_requested,
                session_trust_available,
                ..
            } => {
                confirmations = confirmations.saturating_add(1);
                match confirmations {
                    1 => {
                        assert!(!model_requested);
                        assert!(session_trust_available);
                        commands
                            .send(OrchestratorCommand::Confirm {
                                turn_id,
                                action_id,
                                decision: ShellApprovalDecision::TrustExactForSession,
                            })
                            .await
                            .unwrap();
                    }
                    2 => {
                        assert!(model_requested);
                        assert!(!session_trust_available);
                        commands
                            .send(OrchestratorCommand::Confirm {
                                turn_id,
                                action_id,
                                decision: ShellApprovalDecision::Decline,
                            })
                            .await
                            .unwrap();
                    }
                    unexpected => panic!("unexpected confirmation #{unexpected}"),
                }
            }
            OrchestratorEvent::ToolCompleted { outcome, .. } => match outcome {
                ToolOutcome::Success(_) => {
                    successful_commands = successful_commands.saturating_add(1);
                }
                ToolOutcome::Declined { .. } => {
                    declined_commands = declined_commands.saturating_add(1);
                }
                ToolOutcome::Failure { message } => {
                    panic!("command unexpectedly failed: {message}")
                }
            },
            OrchestratorEvent::Done { turn_id } => {
                assert_eq!(turn_id, 1);
                saw_done = true;
            }
            _ => {}
        }
    }

    assert_eq!(
        confirmations, 2,
        "the repeated local-policy command should be silent"
    );
    assert_eq!(successful_commands, 2);
    assert_eq!(declined_commands, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn hard_whip_retries_turn_and_applies_three_response_penalty() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("too slow"),
                completed_sse("retried quickly"),
            ]),
            first_delay: Duration::from_secs(2),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 2)).await;
    commands.submit("answer now").await.unwrap();

    loop {
        if matches!(
            next_orchestrator_event(&mut events).await,
            OrchestratorEvent::PhaseChanged {
                phase: decode::agent::AgentPhase::Requesting,
                ..
            }
        ) {
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    commands.submit("must be rejected").await.unwrap();
    commands
        .send(OrchestratorCommand::Whip { turn_id: 999 })
        .await
        .unwrap();
    commands
        .send(OrchestratorCommand::Whip { turn_id: 1 })
        .await
        .unwrap();
    commands
        .send(OrchestratorCommand::Whip { turn_id: 1 })
        .await
        .unwrap();

    let mut busy_rejected = false;
    let mut whip_kinds = Vec::new();
    let mut done = false;
    while !done {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::BusyRejected { .. } => busy_rejected = true,
            OrchestratorEvent::WhipAcknowledged { kind, .. } => {
                whip_kinds.push(kind);
            }
            OrchestratorEvent::Done { .. } => done = true,
            _ => {}
        }
    }
    assert!(busy_rejected);
    assert_eq!(whip_kinds, vec![WhipKind::Soft, WhipKind::Hard]);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let retried_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(retried_body["reasoning"]["effort"], "low");
    assert_eq!(retried_body["max_output_tokens"], 614);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn tool_iteration_limit_blocks_until_stop_decision() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let read = "<read_file><path>a.txt</path></read_file>";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![completed_sse(read), completed_sse(read)]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("a.txt"), "contents").unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 1)).await;
    commands.submit("read twice").await.unwrap();

    let mut tool_starts = 0_u32;
    let continuation_id = loop {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::ToolStarted { .. } => {
                tool_starts = tool_starts.saturating_add(1);
            }
            OrchestratorEvent::ContinuationRequested {
                turn_id,
                continuation_id,
                completed_iterations,
                max_iterations,
            } => {
                assert_eq!(turn_id, 1);
                assert_eq!(completed_iterations, 1);
                assert_eq!(max_iterations, 1);
                break continuation_id;
            }
            _ => {}
        }
    };
    commands
        .send(OrchestratorCommand::ContinueToolLoop {
            turn_id: 1,
            continuation_id,
            continue_loop: false,
        })
        .await
        .unwrap();

    loop {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::ToolStarted { .. } => {
                tool_starts = tool_starts.saturating_add(1);
            }
            OrchestratorEvent::Done { .. } => break,
            _ => {}
        }
    }
    assert_eq!(tool_starts, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn duplicate_continuation_id_cannot_confirm_the_next_modal_of_the_same_turn() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let read = "<read_file><path>seed.txt</path></read_file>";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse(read),
                completed_sse(read),
                completed_sse(read),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("seed.txt"), "seed").unwrap();
    let (commands, mut events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 1),
        64,
    )
    .await;
    commands
        .submit("exercise two continuation modals")
        .await
        .unwrap();

    let first = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::AwaitingContinuation)
    })
    .await;
    let first_id = match first.modal {
        Some(UiModal::Continuation {
            continuation_id, ..
        }) => continuation_id,
        _ => panic!("first continuation modal did not carry its ID"),
    };
    commands
        .send(OrchestratorCommand::ContinueToolLoop {
            turn_id: 1,
            continuation_id: first_id,
            continue_loop: true,
        })
        .await
        .unwrap();

    let second = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::AwaitingContinuation)
            && matches!(
                snapshot.modal,
                Some(UiModal::Continuation {
                    continuation_id,
                    ..
                }) if continuation_id != first_id
            )
    })
    .await;
    let second_id = match second.modal {
        Some(UiModal::Continuation {
            continuation_id, ..
        }) => continuation_id,
        _ => panic!("second continuation modal did not carry its ID"),
    };
    let second_scope = CommandScope {
        conversation_epoch: second.conversation_epoch,
        phase_revision: second.phase_revision,
    };

    // FIFO is the acknowledgement barrier: BusyRejected for the scoped
    // Submit proves the preceding stale decision was consumed. It must not
    // dismiss the second modal even though turn_id is unchanged.
    commands
        .send(OrchestratorCommand::ContinueToolLoop {
            turn_id: 1,
            continuation_id: first_id,
            continue_loop: false,
        })
        .await
        .unwrap();
    commands
        .send(OrchestratorCommand::Submit {
            prompt: "FIFO acknowledgement barrier".to_owned(),
            attachments: Vec::new(),
            scope: second_scope,
        })
        .await
        .unwrap();
    loop {
        if matches!(
            next_orchestrator_event(&mut events).await,
            OrchestratorEvent::BusyRejected { .. }
        ) {
            break;
        }
    }
    let still_waiting = snapshots.borrow().clone();
    assert!(matches!(
        still_waiting.modal,
        Some(UiModal::Continuation {
            continuation_id,
            ..
        }) if continuation_id == second_id
    ));
    assert!(matches!(
        still_waiting.phase,
        AgentPhase::AwaitingContinuation
    ));

    commands
        .send(OrchestratorCommand::ContinueToolLoop {
            turn_id: 1,
            continuation_id: second_id,
            continue_loop: false,
        })
        .await
        .unwrap();
    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.history.iter().any(|entry| {
                matches!(
                    entry.kind,
                    HistoryKind::ToolResult {
                        outcome: ToolResultStatus::Declined,
                        ..
                    }
                )
            })
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(
        !completed
            .history
            .iter()
            .any(|entry| entry.content.contains("FIFO acknowledgement barrier"))
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn completed_event_requires_explicit_completed_status_and_never_runs_tools() {
    for status in [
        None,
        Some("incomplete"),
        Some("cancelled"),
        Some("future_status"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                completed_sse_with_status("must not commit", status),
                "text/event-stream",
            ))
            .mount(&server)
            .await;

        let error = ResponsesClient::new(api_config(&server))
            .unwrap()
            .completed_response(request(), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ApiError::Protocol(message) if message.contains("non-completed status")),
            "status {status:?} unexpectedly passed the terminal gate"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    let server = MockServer::start().await;
    let unsafe_tool = concat!(
        "<write_file><path>terminal-gate.txt</path>",
        "<content>must never exist</content></write_file>"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_sse_with_status(unsafe_tool, Some("incomplete")),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent_config(workspace.path(), 2), 8)
            .await;
    commands
        .submit("try the invalid terminal tool")
        .await
        .unwrap();
    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Error { .. })
    })
    .await;
    assert!(
        failed
            .history
            .iter()
            .all(|entry| { !matches!(entry.kind, HistoryKind::ToolResult { .. }) })
    );
    assert!(!workspace.path().join("terminal-gate.txt").exists());

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn full_diagnostic_channel_cannot_block_snapshot_reset_or_scoped_submit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("snapshot answer"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _unread_full_events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent_config(workspace.path(), 2), 1)
            .await;

    let initial = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.phase_revision > 0
    })
    .await;
    let stale_scope = CommandScope {
        conversation_epoch: initial.conversation_epoch,
        phase_revision: initial.phase_revision,
    };

    commands.send(OrchestratorCommand::Reset).await.unwrap();
    let reset = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.conversation_epoch == initial.conversation_epoch.saturating_add(1)
    })
    .await;
    assert!(reset.history.is_empty());
    assert!(matches!(reset.phase, AgentPhase::Idle));

    commands
        .send(OrchestratorCommand::Submit {
            prompt: "stale prompt".to_owned(),
            attachments: Vec::new(),
            scope: stale_scope,
        })
        .await
        .unwrap();
    let rejected =
        wait_for_snapshot(&mut snapshots, |snapshot| snapshot.status.contains("stale")).await;
    assert!(rejected.history.is_empty());
    assert!(server.received_requests().await.unwrap().is_empty());

    let current_scope = CommandScope {
        conversation_epoch: rejected.conversation_epoch,
        phase_revision: rejected.phase_revision,
    };
    commands
        .send(OrchestratorCommand::Submit {
            prompt: "current prompt".to_owned(),
            attachments: Vec::new(),
            scope: current_scope,
        })
        .await
        .unwrap();
    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.assistant == "snapshot answer"
            && snapshot
                .history
                .iter()
                .any(|entry| matches!(entry.kind, HistoryKind::Assistant))
    })
    .await;
    assert_eq!(completed.conversation_epoch, reset.conversation_epoch);
    assert!(
        completed
            .history
            .iter()
            .all(|entry| matches!(entry.status, HistoryStatus::Committed))
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 1);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn submit_scoped_while_busy_is_rejected_when_completion_becomes_ready() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let ready = Arc::new(AtomicUsize::new(0));
    let thread_ready = ready.clone();
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
    let response_body = completed_sse("only the first turn");
    let server_thread = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut request = Vec::new();
        let mut buffer = [0_u8; 2_048];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending HTTP headers");
            request.extend_from_slice(&buffer[..read]);
        }
        let header_end = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
            .unwrap();
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        while request.len() < header_end.saturating_add(content_length) {
            let read = socket.read(&mut buffer).unwrap();
            assert!(read > 0, "client closed before sending the HTTP body");
            request.extend_from_slice(&buffer[..read]);
        }
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            response_body.len()
        )
        .unwrap();
        socket.flush().unwrap();
        thread_ready.store(1, Ordering::SeqCst);
        release_rx.recv().unwrap();
        socket.write_all(response_body.as_bytes()).unwrap();
        socket.flush().unwrap();
    });

    let workspace = TempDir::new().unwrap();
    let dummy_server = MockServer::start().await;
    let mut api = api_config(&dummy_server);
    api.endpoint = ResponsesEndpoint::FullUrl(format!("http://{address}/responses"));
    api.request_timeout = Duration::from_secs(5);
    api.stream_idle_timeout = Duration::from_secs(5);
    api.max_attempts = 1;
    let (commands, mut events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api, agent_config(workspace.path(), 2), 64).await;
    commands.submit("first prompt").await.unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        while ready.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mock server never exposed the response headers");
    let busy = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Streaming)
    })
    .await;
    let busy_scope = CommandScope {
        conversation_epoch: busy.conversation_epoch,
        phase_revision: busy.phase_revision,
    };

    // Enqueue a scope captured while Streaming, then release the terminal SSE
    // body. Whether the command or response.completed wins the next poll, the
    // prompt must be rejected rather than becoming a hidden follow-up turn.
    commands
        .send(OrchestratorCommand::Submit {
            prompt: "must never become a queued turn".to_owned(),
            attachments: Vec::new(),
            scope: busy_scope,
        })
        .await
        .unwrap();
    release_tx.send(()).unwrap();

    let mut rejected = false;
    let mut completed = false;
    while !(rejected && completed) {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::BusyRejected { .. } => rejected = true,
            OrchestratorEvent::Done { turn_id: 1 } => completed = true,
            _ => {}
        }
    }
    let final_snapshot = snapshots.borrow().clone();
    assert!(matches!(final_snapshot.phase, AgentPhase::Idle));
    assert_eq!(final_snapshot.assistant, "only the first turn");
    assert!(final_snapshot.history.iter().any(|entry| {
        matches!(entry.kind, HistoryKind::User) && entry.content == "first prompt"
    }));
    assert!(
        !final_snapshot
            .history
            .iter()
            .any(|entry| { entry.content.contains("must never become a queued turn") })
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
    server_thread.join().unwrap();
}

#[tokio::test]
async fn explicit_queue_runs_fifo_as_a_separate_turn_after_active_completion() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("first complete"),
                completed_sse("queued complete"),
            ]),
            first_delay: Duration::from_millis(400),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let mut api = api_config(&server);
    api.request_timeout = Duration::from_secs(3);
    api.stream_idle_timeout = Duration::from_secs(3);
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api, agent_config(workspace.path(), 3), 64).await;
    commands.submit("first prompt").await.unwrap();
    let requesting = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Requesting) && snapshot.active_turn_id == Some(1)
    })
    .await;
    commands
        .send(OrchestratorCommand::EnqueueFollowUp {
            mode: FollowUpMode::Queue,
            text: "second prompt in FIFO".to_owned(),
            scope: CommandScope {
                conversation_epoch: requesting.conversation_epoch,
                phase_revision: requesting.phase_revision,
            },
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.history.iter().any(|entry| {
                matches!(entry.kind, HistoryKind::User) && entry.content == "second prompt in FIFO"
            })
            && snapshot.follow_ups.items.iter().any(|item| {
                item.text == "second prompt in FIFO" && item.status == FollowUpStatus::Delivered
            })
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let user_prompts = completed
        .history
        .iter()
        .filter(|entry| matches!(entry.kind, HistoryKind::User))
        .map(|entry| entry.content.as_str())
        .collect::<Vec<_>>();
    assert_eq!(user_prompts, ["first prompt", "second prompt in FIFO"]);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn plan_pass_usage_is_included_in_the_session_ledger() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("Inspect the implementation, then make the smallest safe change."),
                completed_sse("Implementation complete."),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::SetPlanMode {
            enabled: true,
            scope,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| snapshot.work_modes.plan).await;
    commands.submit("Implement the change").await.unwrap();

    let pending = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.modal, Some(UiModal::PlanApproval { .. }))
    })
    .await;
    let UiModal::PlanApproval { review } = pending.modal.unwrap() else {
        unreachable!();
    };
    commands
        .send(OrchestratorCommand::DecidePlan {
            turn_id: review.turn_id,
            review_id: review.review_id,
            decision: PlanDecision::Approve {
                plan: review.plan.clone(),
            },
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.assistant == "Implementation complete."
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let usage = completed.usage.unwrap().usage;
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.output_tokens, 10);
    assert_eq!(usage.total_tokens, 30);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn plan_phase_retries_a_transient_failure_before_output() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(FailThenSucceed {
            calls: calls.clone(),
            failure_status: 503,
            retry_after: Some("0"),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::SetPlanMode {
            enabled: true,
            scope,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| snapshot.work_modes.plan).await;
    commands.submit("Plan the change").await.unwrap();

    let pending = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.modal, Some(UiModal::PlanApproval { .. }))
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let UiModal::PlanApproval { review } = pending.modal.unwrap() else {
        unreachable!();
    };
    commands
        .send(OrchestratorCommand::DecidePlan {
            turn_id: review.turn_id,
            review_id: review.review_id,
            decision: PlanDecision::Reject,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
    })
    .await;

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn blocked_side_tool_call_still_counts_provider_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_sse_with_items(
                "side-tool-call",
                "",
                vec![serde_json::json!({
                    "type": "function_call",
                    "call_id": "side-call",
                    "name": "read_file",
                    "arguments": "{\"path\":\"secret.txt\"}"
                })],
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::AskSideQuestion {
            question: "What changed?".to_owned(),
            deployment: "test-deployment".to_owned(),
            reasoning_effort: ReasoningEffort::Low,
            scope,
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.side_task_generation == 0
            && snapshot
                .side_chat
                .latest()
                .is_some_and(|exchange| exchange.status == SideExchangeStatus::Failed)
    })
    .await;
    assert_eq!(completed.usage.unwrap().usage.total_tokens, 15);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn invisible_side_answer_is_reported_as_empty() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_sse("\u{200b}\u{2060}"), "text/event-stream"),
        )
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::AskSideQuestion {
            question: "What changed?".to_owned(),
            deployment: "test-deployment".to_owned(),
            reasoning_effort: ReasoningEffort::Low,
            scope,
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.side_task_generation == 0
            && snapshot
                .side_chat
                .latest()
                .is_some_and(|exchange| exchange.status == SideExchangeStatus::Failed)
    })
    .await;
    let exchange = completed.side_chat.latest().unwrap();
    assert!(exchange.answer.is_empty());

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn malformed_native_call_still_counts_provider_usage() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            completed_sse_with_items(
                "malformed-call",
                "",
                vec![serde_json::json!({
                    "type": "function_call",
                    "call_id": "broken-call",
                    "arguments": "{}"
                })],
            ),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    commands.submit("Inspect the file").await.unwrap();

    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    assert_eq!(failed.usage.unwrap().usage.total_tokens, 15);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn incomplete_response_still_counts_provider_usage() {
    let server = MockServer::start().await;
    let event = serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": "incomplete-with-usage",
            "status": "incomplete",
            "created_at": 124,
            "output": [],
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "total_tokens": 15
            }
        }
    });
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {event}\n\ndata: [DONE]\n\n"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    commands.submit("Produce a response").await.unwrap();

    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    assert_eq!(failed.usage.unwrap().usage.total_tokens, 15);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn goal_mode_fails_closed_when_the_model_ignores_its_progress_guard() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("Finished without checking the goal."),
                completed_sse("Still finished without checking the goal."),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::SetGoal {
            objective: Some("Finish the audited change".to_owned()),
            scope,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.work_modes.goal.is_some()
    })
    .await;
    commands.submit("Do the work").await.unwrap();

    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(failed.history.iter().any(|entry| {
        matches!(entry.kind, HistoryKind::ToolResult { .. })
            && entry.content.contains("Goal Mode requires one update_goal")
    }));

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn review_mode_completes_only_after_snapshot_bound_structured_report() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let empty_digest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    let submit_arguments = serde_json::json!({
        "snapshot_sha256": empty_digest,
        "verdict": "pass",
        "summary": "No concrete defect was introduced by the empty captured diff.",
        "findings": []
    })
    .to_string();
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items(
                    "review-page",
                    "",
                    vec![serde_json::json!({
                        "type": "function_call",
                        "call_id": "call_review_diff",
                        "name": "review_diff",
                        "arguments": "{\"offset\":0,\"max_bytes\":1024}"
                    })],
                ),
                completed_sse_with_items(
                    "review-submit",
                    "",
                    vec![serde_json::json!({
                        "type": "function_call",
                        "call_id": "call_submit_review",
                        "name": "submit_review",
                        "arguments": submit_arguments
                    })],
                ),
                completed_sse("Structured review complete."),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let mut agent = agent_config(workspace.path(), 4);
    std::fs::write(workspace.path().join(".gitignore"), ".test-sessions/\n").unwrap();
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "-A"],
        vec![
            "-c",
            "user.name=review-test",
            "-c",
            "user.email=review@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(workspace.path())
                .status()
                .unwrap()
                .success()
        );
    }
    // Keep runtime journals outside the reviewed worktree so the expected
    // snapshot is genuinely empty and deterministically hashes to SHA-256("").
    let session_directory = TempDir::new().unwrap();
    agent.session_dir = session_directory.path().join("sessions");
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;
    let scope = {
        let snapshot = snapshots.borrow();
        CommandScope {
            conversation_epoch: snapshot.conversation_epoch,
            phase_revision: snapshot.phase_revision,
        }
    };
    commands
        .send(OrchestratorCommand::SetReviewMode {
            enabled: true,
            scope,
        })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| snapshot.work_modes.review).await;
    commands
        .submit("Review the current Git changes")
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot.reviews.reports.len() == 1
            && snapshot.assistant == "Structured review complete."
    })
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let report = completed.reviews.latest().unwrap();
    assert_eq!(report.snapshot_sha256, empty_digest);
    assert_eq!(report.verdict, ReviewVerdict::Pass);
    assert!(report.findings.is_empty());

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn retry_turn_reuses_one_pending_prompt_and_excludes_failed_draft() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_status("discarded draft", Some("incomplete")),
                completed_sse_with_items("retry-success", "retried answer", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 2),
        32,
    )
    .await;
    commands.submit("retry this exact prompt").await.unwrap();

    let failed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    assert_eq!(
        failed
            .history
            .iter()
            .filter(|entry| matches!(entry.kind, HistoryKind::User))
            .count(),
        1
    );
    assert!(failed.history.iter().any(|entry| {
        matches!(entry.kind, HistoryKind::User) && matches!(entry.status, HistoryStatus::Pending)
    }));

    commands
        .send(OrchestratorCommand::RetryTurn { turn_id: 1 })
        .await
        .unwrap();
    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "retried answer"
    })
    .await;
    let users: Vec<_> = completed
        .history
        .iter()
        .filter(|entry| matches!(entry.kind, HistoryKind::User))
        .collect();
    assert_eq!(users.len(), 1);
    assert_eq!(users[0].content, "retry this exact prompt");
    assert!(matches!(users[0].status, HistoryStatus::Committed));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let metrics = completed
        .history
        .iter()
        .rev()
        .find_map(|entry| entry.turn_metrics.as_ref())
        .expect("completed retry must expose metrics for the full logical turn");
    assert_eq!(metrics.input_tokens, 20);
    assert_eq!(metrics.output_tokens, 10);
    assert_eq!(metrics.total_tokens, 30);

    let requests = server.received_requests().await.unwrap();
    let retried: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let input = retried["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"], "retry this exact prompt");

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn pause_resume_metrics_cover_the_entire_logical_turn() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let read = "<read_file><path>pause.txt</path></read_file>";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SelectivelyDelayedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items("before-pause", read, Vec::new()),
                completed_sse_with_items("cancelled", "not committed", Vec::new()),
                completed_sse_with_items("after-pause", "resumed answer", Vec::new()),
            ]),
            delayed_indices: Arc::new(vec![0, 1]),
            delay: Duration::from_millis(200),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("pause.txt"), "contents").unwrap();
    let (event_tx, mut events) = mpsc::channel(64);
    let (command_tx, command_rx) = mpsc::channel(64);
    let (orchestrator, snapshots, urgent) = Orchestrator::with_runtime(
        api_config(&server),
        agent_config(workspace.path(), 3),
        event_tx,
        command_rx,
    )
    .unwrap();
    let task = tokio::spawn(orchestrator.run());
    let mut command_snapshots = snapshots.clone();
    wait_for_initial_scope(&mut command_snapshots).await;
    let commands = TestCommandSender::new(command_tx, command_snapshots);
    let mut snapshots = snapshots;
    commands.submit("read then finish").await.unwrap();

    let mut tool_completed = false;
    loop {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::ToolCompleted { .. } => tool_completed = true,
            OrchestratorEvent::PhaseChanged {
                turn_id: Some(1),
                phase: AgentPhase::Requesting,
            } if tool_completed => break,
            _ => {}
        }
    }
    urgent.pause(1);
    wait_for_snapshot(&mut snapshots, |snapshot| {
        snapshot.paused_turn_id == Some(1)
    })
    .await;
    commands
        .send(OrchestratorCommand::RetryTurn { turn_id: 1 })
        .await
        .unwrap();
    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "resumed answer"
    })
    .await;

    let metrics = completed
        .history
        .iter()
        .rev()
        .find_map(|entry| entry.turn_metrics.as_ref())
        .expect("resumed turn must expose aggregate metrics");
    assert_eq!(metrics.input_tokens, 20);
    assert_eq!(metrics.output_tokens, 10);
    assert_eq!(metrics.total_tokens, 30);
    assert!(metrics.elapsed_millis >= 150);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn e2e_stateless_replays_opaque_items_and_applies_write_then_patch() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let write = concat!(
        "<write_file><path>artifact.txt</path>",
        "<content>alpha</content></write_file>"
    );
    let patch = concat!(
        "<apply_patch><path>artifact.txt</path>",
        "<search>alpha</search><replace>beta</replace></apply_patch>"
    );
    let opaque_reasoning = serde_json::json!({
        "type": "reasoning",
        "id": "reasoning-opaque",
        "encrypted_content": "ciphertext-must-round-trip",
        "summary": []
    });
    let first_message = serde_json::json!({
        "type": "message",
        "id": "write-message",
        "role": "assistant",
        "status": "completed",
        "content": [{"type": "output_text", "text": write}]
    });
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items(
                    "write-response",
                    write,
                    vec![opaque_reasoning.clone(), first_message],
                ),
                completed_sse_with_items("patch-response", patch, Vec::new()),
                completed_sse_with_items("final-response", "All changes applied.", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    if Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        let initialized = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .unwrap();
        assert!(initialized.success());
        assert!(workspace.path().join(".git").is_dir());
    }
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 4),
        64,
    )
    .await;
    commands
        .submit("create and update artifact.txt")
        .await
        .unwrap();

    let approval = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::AwaitingPatchApproval)
    })
    .await;
    let (turn_id, action_id, hunk_count) = match approval.modal {
        Some(UiModal::PatchApproval {
            turn_id,
            action_id,
            review,
        }) => (turn_id, action_id, review.hunks.len()),
        _ => panic!("patch approval snapshot did not carry its scoped review"),
    };
    commands
        .send(OrchestratorCommand::DecidePatch {
            turn_id,
            action_id,
            decisions: vec![true; hunk_count],
        })
        .await
        .unwrap();

    let patch_approval = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            &snapshot.modal,
            Some(UiModal::PatchApproval {
                action_id: next_action,
                ..
            }) if *next_action != action_id
        )
    })
    .await;
    let (patch_turn_id, patch_action_id, patch_hunks) = match patch_approval.modal {
        Some(UiModal::PatchApproval {
            turn_id,
            action_id,
            review,
        }) => (turn_id, action_id, review.hunks.len()),
        _ => panic!("apply_patch approval snapshot did not carry its scoped review"),
    };
    commands
        .send(OrchestratorCommand::DecidePatch {
            turn_id: patch_turn_id,
            action_id: patch_action_id,
            decisions: vec![true; patch_hunks],
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "All changes applied."
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("artifact.txt")).unwrap(),
        "beta"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert!(completed.history.iter().any(|entry| {
        matches!(
            &entry.kind,
            HistoryKind::ToolResult {
                tool_name,
                outcome: ToolResultStatus::Success,
                ..
            } if tool_name == "write_file"
        )
    }));
    assert!(completed.history.iter().any(|entry| {
        matches!(
            &entry.kind,
            HistoryKind::ToolResult {
                tool_name,
                outcome: ToolResultStatus::Success,
                ..
            } if tool_name == "apply_patch"
        )
    }));

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let second_instructions = second["instructions"].as_str().unwrap();
    assert!(second_instructions.starts_with("Use the tagged tool protocol exactly.\n"));
    assert!(second_instructions.contains("Read batching policy"));
    let second_input = second["input"].as_array().unwrap();
    assert!(second_input.iter().any(|item| item == &opaque_reasoning));
    let write_result = second_input.iter().find_map(|item| {
        let content = item.get("content")?.as_str()?;
        let envelope: serde_json::Value = serde_json::from_str(content).ok()?;
        (envelope.get("tool")?.as_str()? == "write_file").then_some(envelope)
    });
    let write_result = write_result.expect("write ToolResult missing from second round");
    assert_eq!(write_result["status"], "success");

    let third: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(third["instructions"].as_str(), Some(second_instructions));
    let patch_result = third["input"].as_array().unwrap().iter().find_map(|item| {
        let content = item.get("content")?.as_str()?;
        let envelope: serde_json::Value = serde_json::from_str(content).ok()?;
        (envelope.get("tool")?.as_str()? == "apply_patch").then_some(envelope)
    });
    assert_eq!(
        patch_result.expect("patch ToolResult missing")["status"],
        "success"
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn stateful_follow_up_sends_only_unsent_tool_result_with_exact_cursor() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let read = "<read_file><path>stateful.txt</path></read_file>";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items("stateful-first", read, Vec::new()),
                completed_sse_with_items("stateful-final", "stateful done", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("stateful.txt"), "stateful contents").unwrap();
    let mut agent = agent_config(workspace.path(), 3);
    agent.context_mode = ContextMode::Stateful;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 32).await;
    commands.submit("read stateful.txt").await.unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "stateful done"
    })
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let requests = server.received_requests().await.unwrap();
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(first["store"], true);
    assert!(first.get("previous_response_id").is_none());

    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(second["previous_response_id"], "stateful-first");
    assert_eq!(second["store"], true);
    let input = second["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["role"], "user");
    let tool_result: serde_json::Value =
        serde_json::from_str(input[0]["content"].as_str().unwrap()).unwrap();
    assert_eq!(tool_result["tool"], "read_file");
    assert_eq!(tool_result["status"], "success");

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn stopping_iteration_limit_closes_every_unexecuted_action() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let first = "<read_file><path>seed.txt</path></read_file>";
    let skipped = concat!(
        "<write_file><path>skipped-a.txt</path><content>a</content></write_file>",
        "<write_file><path>skipped-b.txt</path><content>b</content></write_file>"
    );
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items("limit-first", first, Vec::new()),
                completed_sse_with_items("limit-second", skipped, Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("seed.txt"), "seed").unwrap();
    let (commands, _events, mut snapshots, task) = start_orchestrator_with_snapshot(
        api_config(&server),
        agent_config(workspace.path(), 1),
        64,
    )
    .await;
    commands.submit("reach the tool limit").await.unwrap();
    let awaiting = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::AwaitingContinuation)
    })
    .await;
    let continuation_id = match awaiting.modal {
        Some(UiModal::Continuation {
            continuation_id, ..
        }) => continuation_id,
        _ => panic!("continuation snapshot did not carry its one-shot ID"),
    };
    commands
        .send(OrchestratorCommand::ContinueToolLoop {
            turn_id: 1,
            continuation_id,
            continue_loop: false,
        })
        .await
        .unwrap();

    let completed = wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle)
            && snapshot
                .history
                .iter()
                .filter(|entry| {
                    matches!(
                        entry.kind,
                        HistoryKind::ToolResult {
                            outcome: ToolResultStatus::Declined,
                            ..
                        }
                    )
                })
                .count()
                == 2
    })
    .await;
    let declined_names: Vec<_> = completed
        .history
        .iter()
        .filter_map(|entry| match &entry.kind {
            HistoryKind::ToolResult {
                tool_name,
                outcome: ToolResultStatus::Declined,
                ..
            } => Some(tool_name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(declined_names, vec!["write_file", "write_file"]);
    assert!(!workspace.path().join("skipped-a.txt").exists());
    assert!(!workspace.path().join("skipped-b.txt").exists());
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn whip_penalty_applies_to_exactly_three_completed_responses() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items("aborted", "aborted", Vec::new()),
                completed_sse_with_items("penalty-one", "one", Vec::new()),
                completed_sse_with_items("penalty-two", "two", Vec::new()),
                completed_sse_with_items("penalty-three", "three", Vec::new()),
                completed_sse_with_items("normal-four", "four", Vec::new()),
            ]),
            first_delay: Duration::from_secs(2),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 2)).await;
    commands.submit("first logical turn").await.unwrap();
    loop {
        if matches!(
            next_orchestrator_event(&mut events).await,
            OrchestratorEvent::PhaseChanged {
                phase: AgentPhase::Requesting,
                ..
            }
        ) {
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    commands
        .send(OrchestratorCommand::Whip { turn_id: 1 })
        .await
        .unwrap();

    let mut saw_soft = false;
    loop {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::WhipAcknowledged {
                kind: WhipKind::Soft,
                ..
            } => saw_soft = true,
            OrchestratorEvent::Done { turn_id: 1 } => break,
            _ => {}
        }
    }
    assert!(saw_soft);

    for turn_id in 2..=4 {
        commands
            .submit(format!("logical turn {turn_id}"))
            .await
            .unwrap();
        loop {
            if matches!(
                next_orchestrator_event(&mut events).await,
                OrchestratorEvent::Done { turn_id: done } if done == turn_id
            ) {
                break;
            }
        }
    }

    assert_eq!(calls.load(Ordering::SeqCst), 5);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 5);
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    let efforts: Vec<_> = bodies
        .iter()
        .map(|body| body["reasoning"]["effort"].as_str().unwrap())
        .collect();
    assert_eq!(efforts, vec!["medium", "low", "low", "low", "medium"]);
    let limits: Vec<_> = bodies
        .iter()
        .map(|body| body["max_output_tokens"].as_u64().unwrap())
        .collect();
    assert_eq!(limits, vec![1_024, 614, 614, 614, 1_024]);
    let developer_note_counts: Vec<_> = bodies
        .iter()
        .map(|body| {
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "developer")
                .count()
        })
        .collect();
    assert_eq!(developer_note_counts, vec![0, 1, 0, 0, 0]);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn sse_rejects_a_whole_turn_over_eight_mib_even_with_small_frames() {
    let payload_bytes = 900_000_usize;
    let frame_count = MAX_SSE_TURN_BYTES / payload_bytes + 2;
    let chunks: Vec<_> = (0..frame_count)
        .map(|_| {
            let mut frame = Vec::with_capacity(payload_bytes + 3);
            frame.push(b':');
            frame.extend(std::iter::repeat_n(b'x', payload_bytes));
            frame.extend_from_slice(b"\n\n");
            Ok::<_, ApiError>(Bytes::from(frame))
        })
        .collect();
    let events: Vec<_> = parse_sse_stream(stream::iter(chunks)).collect().await;
    assert!(matches!(
        events.last(),
        Some(Err(ApiError::Protocol(message))) if message.contains("SSE turn exceeded")
    ));
}

#[tokio::test]
async fn stalled_authentication_error_bodies_are_never_read_or_retried() {
    for status in [401_u16, 403_u16] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request_bytes = [0_u8; 8_192];
            let _ = socket.read(&mut request_bytes);
            write!(
                socket,
                "HTTP/1.1 {status} blocked\r\nContent-Length: 999999\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            socket.flush().unwrap();
            thread::sleep(Duration::from_secs(1));
        });

        let config = ApiConfig {
            provider: ApiProvider::Azure,
            auth: ApiAuth::ApiKey,
            api_key: SecretString::new("test-secret".into()),
            bedrock_runtime: decode::config::BedrockRuntimeConfig::default(),
            transport: decode::config::ApiTransport::Sse,
            endpoint: ResponsesEndpoint::FullUrl(format!("http://{address}/responses")),
            allow_insecure_loopback: true,
            deployment: "test-deployment".to_owned(),
            deployment_choices: vec!["test-deployment".to_owned()],
            api_version: None,
            max_output_tokens: 1_024,
            reasoning_effort: ReasoningEffort::Medium,
            temperature: None,
            server_compaction_threshold: None,
            request_timeout: Duration::from_secs(2),
            stream_idle_timeout: Duration::from_secs(30),
            max_attempts: 5,
            retry_min_delay: Duration::from_millis(1),
            retry_max_delay: Duration::from_millis(1),
            retry_after_cap: Duration::from_secs(120),
            pricing: decode::usage::PricingCatalog::default(),
            pricing_catalog_url: None,
        };
        let client = ResponsesClient::new(config).unwrap();
        let error = tokio::time::timeout(
            Duration::from_millis(500),
            client.completed_response(request(), CancellationToken::new()),
        )
        .await
        .expect("client waited for a stalled authentication body")
        .unwrap_err();
        assert!(matches!(error, ApiError::Http { status: actual, .. } if actual == status));
        server.join().unwrap();
    }
}

#[tokio::test]
async fn stateful_cursor_does_not_advance_after_incomplete_response() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let read = "<read_file><path>cursor.txt</path></read_file>";
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse_with_items("cursor-stable", read, Vec::new()),
                incomplete_sse("must-not-become-cursor", "incomplete"),
                completed_sse_with_items("cursor-final", "recovered", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("cursor.txt"), "cursor contents").unwrap();
    let mut agent = agent_config(workspace.path(), 3);
    agent.context_mode = ContextMode::Stateful;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 64).await;
    commands.submit("read cursor.txt").await.unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(
            snapshot.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        )
    })
    .await;
    commands
        .send(OrchestratorCommand::RetryTurn { turn_id: 1 })
        .await
        .unwrap();
    wait_for_snapshot(&mut snapshots, |snapshot| {
        matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "recovered"
    })
    .await;

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let failed_round: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let retried_round: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(failed_round["previous_response_id"], "cursor-stable");
    assert_eq!(retried_round["previous_response_id"], "cursor-stable");
    assert_eq!(failed_round["input"], retried_round["input"]);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn ui_history_snapshot_never_exceeds_512_entries() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let parse_errors = "<read_file></read_file>".repeat(520);
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls,
            bodies: Arc::new(vec![
                completed_sse_with_items("many-errors", &parse_errors, Vec::new()),
                completed_sse_with_items("many-errors-final", "done", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 120_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 1).await;
    commands
        .submit("generate bounded diagnostics")
        .await
        .unwrap();
    let completed =
        wait_for_snapshot_with_timeout(&mut snapshots, Duration::from_secs(15), |snapshot| {
            matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "done"
        })
        .await;
    assert!(completed.history.len() <= 512);
    assert!(
        completed
            .history
            .iter()
            .any(|entry| { entry.content.contains("older history entries summarized") })
    );

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn ui_history_snapshot_never_exceeds_two_mib_of_content() {
    const UI_LIMIT: usize = 2 * 1024 * 1024;
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let reads = "<read_file><path>large.txt</path></read_file>".repeat(20);
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SequencedSse {
            calls,
            bodies: Arc::new(vec![
                completed_sse_with_items("many-large-results", &reads, Vec::new()),
                completed_sse_with_items("many-large-final", "done", Vec::new()),
            ]),
            first_delay: Duration::ZERO,
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    std::fs::write(workspace.path().join("large.txt"), "x".repeat(128 * 1024)).unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.context_budget = 1_000_000;
    let (commands, _events, mut snapshots, task) =
        start_orchestrator_with_snapshot(api_config(&server), agent, 1).await;
    commands
        .submit("generate bounded large results")
        .await
        .unwrap();
    let completed =
        wait_for_snapshot_with_timeout(&mut snapshots, Duration::from_secs(15), |snapshot| {
            matches!(snapshot.phase, AgentPhase::Idle) && snapshot.assistant == "done"
        })
        .await;
    let visible_bytes: usize = completed
        .history
        .iter()
        .map(|entry| entry.content.len().saturating_add(128))
        .sum();
    assert!(visible_bytes <= UI_LIMIT, "visible bytes={visible_bytes}");
    assert!(completed.history.len() <= 512);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn whip_double_hit_window_expiry_keeps_second_strike_soft() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SelectivelyDelayedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("aborted one"),
                completed_sse("aborted two"),
                completed_sse("after expiry"),
            ]),
            delayed_indices: Arc::new(vec![0, 1]),
            delay: Duration::from_secs(2),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let mut agent = agent_config(workspace.path(), 2);
    agent.whip.double_hit_window = Duration::from_millis(10);
    let (commands, mut events, task) = start_orchestrator(api_config(&server), agent).await;
    commands
        .submit("expire the double-hit window")
        .await
        .unwrap();
    wait_for_call_count(&calls, 1).await;
    commands
        .send(OrchestratorCommand::Whip { turn_id: 1 })
        .await
        .unwrap();
    wait_for_call_count(&calls, 2).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    commands
        .send(OrchestratorCommand::Whip { turn_id: 1 })
        .await
        .unwrap();

    let mut kinds = Vec::new();
    loop {
        match next_orchestrator_event(&mut events).await {
            OrchestratorEvent::WhipAcknowledged { kind, .. } => kinds.push(kind),
            OrchestratorEvent::Done { turn_id: 1 } => break,
            _ => {}
        }
    }
    assert_eq!(kinds, vec![WhipKind::Soft, WhipKind::Soft]);
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn first_whip_of_each_new_logical_turn_is_soft() {
    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(SelectivelyDelayedSse {
            calls: calls.clone(),
            bodies: Arc::new(vec![
                completed_sse("turn one aborted"),
                completed_sse("turn one done"),
                completed_sse("turn two aborted"),
                completed_sse("turn two done"),
            ]),
            delayed_indices: Arc::new(vec![0, 2]),
            delay: Duration::from_secs(2),
        })
        .mount(&server)
        .await;

    let workspace = TempDir::new().unwrap();
    let (commands, mut events, task) =
        start_orchestrator(api_config(&server), agent_config(workspace.path(), 2)).await;
    let mut kinds = Vec::new();
    for turn_id in 1..=2 {
        commands
            .submit(format!("logical turn {turn_id}"))
            .await
            .unwrap();
        wait_for_call_count(&calls, usize::try_from(turn_id * 2 - 1).unwrap()).await;
        commands
            .send(OrchestratorCommand::Whip { turn_id })
            .await
            .unwrap();
        loop {
            match next_orchestrator_event(&mut events).await {
                OrchestratorEvent::WhipAcknowledged { kind, .. } => kinds.push(kind),
                OrchestratorEvent::Done { turn_id: done } if done == turn_id => break,
                _ => {}
            }
        }
    }
    assert_eq!(kinds, vec![WhipKind::Soft, WhipKind::Soft]);
    assert_eq!(calls.load(Ordering::SeqCst), 4);

    commands.send(OrchestratorCommand::Shutdown).await.unwrap();
    task.await.unwrap();
}

async fn wait_for_call_count(calls: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.load(Ordering::SeqCst) < expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
