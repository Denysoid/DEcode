use std::{collections::BTreeMap, sync::Arc};
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime},
};

use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use decode::{
    agent::{
        AgentProfileCatalogSnapshot, FollowUpMode, InstructionOrigin, InstructionSetSnapshot,
        InstructionSourceSnapshot, PlanReview, SessionId, SessionSummary, ShellApprovalDecision,
        ShellCommandGrant, ShellPermissionSnapshot, SkillCatalogSnapshot, SkillSource,
        SkillSummary, SubagentFleetSnapshot, SubagentId, SubagentMode, SubagentRecoverySummary,
        SubagentSnapshot, SubagentStatus, WhipKind, WorkModes,
        automation::{
            AutomationSnapshot, AutomationSource, CustomCommandSummary, HookEvent, HookSummary,
        },
        followups::FollowUpState,
        orchestrator::{
            OrchestratorCommand, OrchestratorEvent, UiModal, UiSnapshot, UrgentControlHandle,
        },
        phase::AgentPhase,
        state::{HistoryEntry, HistoryKind, HistoryStatus, ToolResultStatus, TurnMetrics},
    },
    api::{ReasoningEffort, ReasoningMode},
    attachments::{AttachmentSource, AttachmentStore},
    code_index::{CodeIndexSnapshot, CodeIndexState},
    github::{GitHubSnapshot, PullRequestSummary},
    lsp::{LspConnectionState, LspDiagnostic, LspDiagnosticSeverity, LspServerSnapshot},
    mcp::{McpConnectionState, McpServerSnapshot},
    parser::tool_action::{ToolAction, ToolOutcome},
    privacy::{PrivacySnapshot, PrivacySourceSnapshot},
    terminal::{
        TerminalFleetSnapshot, TerminalFrame, TerminalRow, TerminalSessionSnapshot, TerminalSpan,
        TerminalStatus, TerminalStyle,
    },
    tools::{CommandDigest, ConfirmationReason, SandboxRoot},
    ui::{
        agents::{AgentBrowseFocus, AgentHit},
        app::{AppState, PendingConfirmation, PendingContinuation},
        confirm::ConfirmationChoice,
        input::{
            handle_key, handle_key_with_control, handle_mouse, handle_mouse_enabled, handle_paste,
            rect_contains,
        },
        lsp::{LspFocus, LspPane},
        notifications::NotificationKind,
        palette::PaletteMode,
        permissions::PermissionFocus,
        privacy::PrivacyFocus,
        render::{self, sanitize_for_display, strip_service_blocks, truncate_for_display},
        shell::{ShellHit, ShellTab},
    },
    usage::{DeploymentUsageSnapshot, TokenUsage, UsageSnapshot},
};
use ratatui::{Terminal, backend::TestBackend, layout::Rect, style::Color};
use std::convert::Infallible;
use tokio::sync::mpsc;

fn infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => match error {},
    }
}

#[test]
fn unicode_truncation_preserves_codepoint_boundaries() {
    assert_eq!(truncate_for_display("Привет, мир", 7), "Привет…");
    assert_eq!(truncate_for_display("🦀🦀🦀", 2), "🦀…");
    assert_eq!(truncate_for_display("ééé", 2), "é…");
}

#[test]
fn final_assistant_text_hides_service_blocks() {
    let response = concat!(
        "Result\n",
        "<thinking>private reasoning</thinking>\n",
        "<execute_command><command>whoami</command></execute_command>",
        "\nDone"
    );

    let visible = strip_service_blocks(response);
    assert!(visible.contains("Result"));
    assert!(visible.contains("Done"));
    assert!(!visible.contains("private reasoning"));
    assert!(!visible.contains("whoami"));
}

#[test]
fn following_chat_keeps_the_tail_of_a_long_latest_message_visible() {
    let mut state = AppState::new();
    let mut content = (0..60)
        .map(|index| format!("older-line-{index:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    content.push_str("\ntail-marker-8675309");
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 1,
        kind: HistoryKind::Assistant,
        content,
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 100,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    state.shell_ui.follow_output = true;
    let mut terminal = infallible(Terminal::new(TestBackend::new(80, 20)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    assert!(terminal_text(&terminal).contains("tail-marker-8675309"));
}

#[test]
fn display_sanitizer_escapes_ansi_and_bidi_controls() {
    let safe = sanitize_for_display("visible\u{1b}[31m\u{202e}hidden\r");
    assert_eq!(safe, "visible\\x1b[31m<U+202E>hidden\\r");
}

#[test]
fn paste_preserves_lines_and_backspace_removes_one_grapheme() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    handle_paste("first\r\n👩‍💻e\u{301}", &mut state);
    assert_eq!(state.input_buffer, "first\n👩‍💻e\u{301}");
    handle_key(
        KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert_eq!(state.input_buffer, "first\n👩‍💻");
}

#[test]
fn pasted_absolute_image_path_becomes_an_attachment_chip() -> Result<(), Box<dyn std::error::Error>>
{
    let selected = tempfile::tempdir()?;
    let image_path = selected.path().join("snipping-tool.png");
    std::fs::write(&image_path, b"screenshot")?;
    let mut state = AppState::new();

    handle_paste(image_path.to_string_lossy().as_ref(), &mut state);

    assert!(state.input_buffer.is_empty());
    assert_eq!(state.pending_attachments.len(), 1);
    assert_eq!(state.pending_attachments[0].filename, "snipping-tool.png");
    assert!(matches!(
        &state.pending_attachments[0].source,
        AttachmentSource::PastedFile { bytes, filename }
            if bytes.as_ref() == b"screenshot" && filename == "snipping-tool.png"
    ));
    Ok(())
}

#[test]
fn at_after_existing_text_opens_the_file_browser_without_erasing_the_composer() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.workspace_root = std::env::current_dir().unwrap_or_default();
    state.input_buffer = "Прикрепи этот файл ".to_owned();
    state.input_cursor = state.input_buffer.len();

    handle_key(key(KeyCode::Char('@')), &mut state, &command_tx);

    assert_eq!(state.palette_ui.mode(), PaletteMode::Files);
    assert_eq!(state.input_buffer, "Прикрепи этот файл ");
}

#[test]
fn file_browser_attaches_multiple_files_outside_the_workspace()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&workspace)?;
    std::fs::create_dir_all(&downloads)?;
    let document = downloads.join("document.custom");
    let image = downloads.join("image.png");
    std::fs::write(&document, b"complete-document")?;
    std::fs::write(&image, b"complete-image")?;
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.workspace_root = workspace;
    state.input_buffer = "Проверь вложения ".to_owned();
    state.input_cursor = state.input_buffer.len();

    handle_key(key(KeyCode::Char('@')), &mut state, &command_tx);
    state.palette_ui.navigate_files_to(&downloads)?;
    for path in [&document, &image] {
        let index = state
            .palette_ui
            .visible_file_paths()
            .iter()
            .position(|candidate| candidate == path)
            .ok_or("file is absent from browser")?;
        state.palette_ui.select(index);
        handle_key(key(KeyCode::Char(' ')), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    std::fs::remove_file(&document)?;
    std::fs::remove_file(&image)?;

    assert_eq!(state.input_buffer, "Проверь вложения ");
    assert_eq!(state.pending_attachments.len(), 2);
    assert!(state.pending_attachments.iter().any(|attachment| {
        matches!(
            &attachment.source,
            AttachmentSource::PastedFile { bytes, filename }
                if filename == "document.custom" && bytes.as_ref() == b"complete-document"
        )
    }));
    assert!(state.pending_attachments.iter().any(|attachment| {
        matches!(
            &attachment.source,
            AttachmentSource::PastedFile { bytes, filename }
                if filename == "image.png" && bytes.as_ref() == b"complete-image"
        )
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    let OrchestratorCommand::Submit {
        prompt,
        attachments,
        ..
    } = command_rx.try_recv()?
    else {
        panic!("unexpected command");
    };
    assert_eq!(prompt, "Проверь вложения ");
    assert_eq!(attachments.len(), 2);
    assert!(attachments.iter().any(|attachment| {
        matches!(
            attachment,
            AttachmentSource::PastedFile { bytes, filename }
                if filename == "document.custom" && bytes.as_ref() == b"complete-document"
        )
    }));
    assert!(attachments.iter().any(|attachment| {
        matches!(
            attachment,
            AttachmentSource::PastedFile { bytes, filename }
                if filename == "image.png" && bytes.as_ref() == b"complete-image"
        )
    }));
    Ok(())
}

#[test]
fn file_browser_primary_button_opens_the_selected_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let workspace = root.path().join("workspace");
    let nested = workspace.join("nested");
    let nested_file = nested.join("inside.bin");
    std::fs::create_dir_all(&nested)?;
    std::fs::write(&nested_file, b"inside")?;
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.workspace_root = workspace;

    handle_key(key(KeyCode::Char('@')), &mut state, &command_tx);
    let nested_index = state
        .palette_ui
        .visible_file_paths()
        .iter()
        .position(|candidate| candidate == &nested)
        .ok_or("directory is absent from browser")?;
    state.palette_ui.select(nested_index);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert_eq!(state.palette_ui.mode(), PaletteMode::Files);
    assert!(state.palette_ui.visible_file_paths().contains(&nested_file));
    Ok(())
}

#[test]
fn terminal_generated_path_keystrokes_become_an_attachment_chip()
-> Result<(), Box<dyn std::error::Error>> {
    let selected = tempfile::tempdir()?;
    let image_path = selected.path().join("screen clip.png");
    std::fs::write(&image_path, b"screenshot")?;
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();

    for character in image_path.to_string_lossy().chars() {
        handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut state,
            &command_tx,
        );
    }

    assert!(state.input_buffer.is_empty());
    assert_eq!(state.pending_attachments.len(), 1);
    assert_eq!(state.pending_attachments[0].filename, "screen clip.png");
    assert!(matches!(
        &state.pending_attachments[0].source,
        AttachmentSource::PastedFile { bytes, .. } if bytes.as_ref() == b"screenshot"
    ));
    Ok(())
}

#[test]
fn terminal_generated_path_keeps_existing_composer_text() -> Result<(), Box<dyn std::error::Error>>
{
    let selected = tempfile::tempdir()?;
    let image_path = selected.path().join("screen clip.png");
    std::fs::write(&image_path, b"screenshot")?;
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.input_buffer = "Опиши изображение:".to_owned();
    state.input_cursor = state.input_buffer.len();

    for character in image_path.to_string_lossy().chars() {
        handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut state,
            &command_tx,
        );
    }

    assert_eq!(state.input_buffer, "Опиши изображение:");
    assert_eq!(state.pending_attachments.len(), 1);
    assert_eq!(state.pending_attachments[0].filename, "screen clip.png");
    assert!(matches!(
        &state.pending_attachments[0].source,
        AttachmentSource::PastedFile { bytes, .. } if bytes.as_ref() == b"screenshot"
    ));

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    let OrchestratorCommand::Submit {
        prompt,
        attachments,
        ..
    } = command_rx.try_recv()?
    else {
        panic!("unexpected command");
    };
    assert_eq!(prompt, "Опиши изображение:");
    assert!(matches!(
        attachments.as_slice(),
        [AttachmentSource::PastedFile { bytes, .. }] if bytes.as_ref() == b"screenshot"
    ));
    Ok(())
}

#[test]
fn terminal_generated_quoted_path_waits_for_the_closing_quote()
-> Result<(), Box<dyn std::error::Error>> {
    let selected = tempfile::tempdir()?;
    let image_path = selected.path().join("screen clip.png");
    std::fs::write(&image_path, b"screenshot")?;
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.input_buffer = "Опиши изображение:".to_owned();
    state.input_cursor = state.input_buffer.len();
    let pasted = format!("\"{}\"", image_path.display());

    for character in pasted[..pasted.len() - 1].chars() {
        handle_key(
            KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            &mut state,
            &command_tx,
        );
    }
    assert!(state.pending_attachments.is_empty());

    handle_key(
        KeyEvent::new(KeyCode::Char('"'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert_eq!(state.input_buffer, "Опиши изображение:");
    assert_eq!(state.pending_attachments.len(), 1);
    Ok(())
}

#[test]
fn pasted_temp_image_survives_after_the_source_file_is_removed()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let selected = tempfile::tempdir()?;
    let image_path = selected.path().join("clipboard-screen.png");
    let image_bytes = b"temporary-screenshot-bytes";
    std::fs::write(&image_path, image_bytes)?;
    let mut state = AppState::new();

    handle_paste(image_path.to_string_lossy().as_ref(), &mut state);
    std::fs::remove_file(&image_path)?;

    let sandbox = SandboxRoot::open(workspace.path())?;
    let blobs = tempfile::tempdir()?;
    let store = AttachmentStore::open(blobs.path().to_path_buf())?;
    let sources = state
        .pending_attachments
        .iter()
        .map(|attachment| attachment.source.clone())
        .collect::<Vec<_>>();
    let staged = store.stage_many(&sandbox, &sources)?;

    assert_eq!(staged.len(), 1);
    assert_eq!(
        std::fs::read(blobs.path().join(&staged[0].sha256))?,
        image_bytes
    );
    Ok(())
}

#[test]
fn dropping_multiple_quoted_files_attaches_every_file() -> Result<(), Box<dyn std::error::Error>> {
    let selected = tempfile::tempdir()?;
    let image = selected.path().join("screen shot.png");
    let document = selected.path().join("design notes.docx");
    let video = selected.path().join("demo clip.mp4");
    let unknown = selected.path().join("raw payload.custom");
    std::fs::write(&image, b"image")?;
    std::fs::write(&document, b"document")?;
    std::fs::write(&video, b"video")?;
    std::fs::write(&unknown, b"unknown")?;
    let paste = format!(
        "\"{}\" \"{}\"\n\"{}\" \"{}\"",
        image.display(),
        document.display(),
        video.display(),
        unknown.display()
    );
    let mut state = AppState::new();

    handle_paste(&paste, &mut state);

    assert!(state.input_buffer.is_empty());
    assert_eq!(state.pending_attachments.len(), 4);
    assert_eq!(
        state
            .pending_attachments
            .iter()
            .map(|attachment| attachment.filename.as_str())
            .collect::<Vec<_>>(),
        [
            "screen shot.png",
            "design notes.docx",
            "demo clip.mp4",
            "raw payload.custom"
        ]
    );
    let sources = state
        .pending_attachments
        .iter()
        .map(|attachment| attachment.source.clone())
        .collect::<Vec<_>>();
    selected.close()?;
    let workspace = tempfile::tempdir()?;
    let sandbox = SandboxRoot::open(workspace.path())?;
    let blobs = tempfile::tempdir()?;
    let staged =
        AttachmentStore::open(blobs.path().to_path_buf())?.stage_many(&sandbox, &sources)?;
    assert_eq!(staged.len(), 4);
    assert_eq!(staged[3].mime_type, "application/octet-stream");
    Ok(())
}

#[test]
fn dropping_unquoted_files_keeps_apostrophes_in_their_names()
-> Result<(), Box<dyn std::error::Error>> {
    let selected = tempfile::tempdir()?;
    let first = selected.path().join("owner's.png");
    let second = selected.path().join("review.pdf");
    std::fs::write(&first, b"image")?;
    std::fs::write(&second, b"document")?;
    let mut state = AppState::new();

    handle_paste(
        &format!("{} {}", first.display(), second.display()),
        &mut state,
    );

    assert!(state.input_buffer.is_empty());
    assert_eq!(state.pending_attachments.len(), 2);
    assert_eq!(state.pending_attachments[0].filename, "owner's.png");
    Ok(())
}

#[test]
fn large_paste_becomes_a_lossless_text_attachment() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let pasted = "important context\n".repeat(40_000);
    let mut state = AppState::new();

    handle_paste(&pasted, &mut state);

    assert!(state.input_buffer.is_empty());
    assert_eq!(state.pending_attachments.len(), 1);
    assert_eq!(state.pending_attachments[0].kind.label(), "text");

    let sandbox = SandboxRoot::open(workspace.path())?;
    let blobs = tempfile::tempdir()?;
    let store = AttachmentStore::open(blobs.path().to_path_buf())?;
    let staged = store.stage_many(&sandbox, &[state.pending_attachments[0].source.clone()])?;
    assert_eq!(
        std::fs::read(blobs.path().join(&staged[0].sha256))?,
        pasted.as_bytes()
    );
    Ok(())
}

#[test]
fn large_text_received_as_key_input_is_lossless_when_submitted() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let pasted = "meaningful line\n".repeat(9_000);
    let mut state = AppState::new();
    state.input_buffer.clone_from(&pasted);
    state.input_cursor = state.input_buffer.len();

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    let OrchestratorCommand::Submit {
        prompt,
        attachments,
        ..
    } = command_rx
        .try_recv()
        .expect("large paste must be submitted")
    else {
        panic!("unexpected command");
    };
    assert!(prompt.is_empty());
    assert!(matches!(
        attachments.as_slice(),
        [AttachmentSource::PastedFile { bytes, filename }]
            if bytes.as_ref() == pasted.as_bytes() && filename.ends_with(".txt")
    ));
}

#[test]
fn session_picker_and_rename_accept_bracketed_paste() {
    let mut state = AppState::new();
    state.session_ui.open(0);

    handle_paste("release\r\nnotes", &mut state);
    assert_eq!(state.session_ui.query(), "release notes");

    state.session_ui.begin_rename("");
    handle_paste("Release\r\nnotes", &mut state);
    assert_eq!(state.session_ui.rename_buffer(), "Release notes");
}

#[test]
fn invisible_session_title_is_not_submitted() -> Result<(), Box<dyn std::error::Error>> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.sessions = Arc::from([SessionSummary {
        id: serde_json::from_str::<SessionId>("\"session-1\"")?,
        title: "Old title".to_owned(),
        preview: String::new(),
        workspace_root: state.workspace_root.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        pinned: false,
        archived: false,
        history_entries: 0,
        parent_session_id: None,
        recovered_records: 0,
    }]);
    state.session_ui.open(1);
    state.session_ui.begin_rename("\u{200b}\u{2060}");

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn session_mutations_stop_when_the_agent_becomes_busy() -> Result<(), Box<dyn std::error::Error>> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.sessions = Arc::from([SessionSummary {
        id: serde_json::from_str::<SessionId>("\"session-1\"")?,
        title: "Saved".to_owned(),
        preview: String::new(),
        workspace_root: state.workspace_root.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        pinned: false,
        archived: false,
        history_entries: 0,
        parent_session_id: None,
        recovered_records: 0,
    }]);
    state.session_ui.open(1);
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.session_ui.clicked(column, row) == Some(decode::ui::sessions::SessionHit::Primary)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn session_refresh_replaces_a_removed_selection_before_the_next_input()
-> Result<(), Box<dyn std::error::Error>> {
    let mut state = AppState::new();
    let first_id = serde_json::from_str::<SessionId>("\"session-1\"")?;
    let second_id = serde_json::from_str::<SessionId>("\"session-2\"")?;
    let make_session = |id: SessionId, title: &str| SessionSummary {
        id,
        title: title.to_owned(),
        preview: String::new(),
        workspace_root: state.workspace_root.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        pinned: false,
        archived: false,
        history_entries: 0,
        parent_session_id: None,
        recovered_records: 0,
    };
    state.sessions = Arc::from([
        make_session(first_id, "First"),
        make_session(second_id.clone(), "Second"),
    ]);
    state.session_ui.open(state.sessions.len());
    state.session_ui.sync(&state.sessions);

    state.handle_orchestrator_event(OrchestratorEvent::SessionsUpdated {
        sessions: Arc::from([make_session(second_id.clone(), "Second")]),
        current_session_id: None,
    });

    assert_eq!(state.session_ui.selected_session_id(), Some(&second_id));
    Ok(())
}

#[test]
fn view_display_toggles_are_mouse_clickable() -> Result<(), Box<dyn std::error::Error>> {
    let (command_tx, _command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 36)));

    handle_key(key(KeyCode::F(10)), &mut state, &command_tx);
    handle_key(key(KeyCode::Right), &mut state, &command_tx);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(terminal_text(&terminal).contains("[x] Show Pixel"));
    let (column, row) = find_ascii_in_buffer(
        terminal.backend().buffer(),
        "[x] Show model-emitted thinking",
    )
    .ok_or("missing clickable thinking toggle")?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(!state.show_thinking);
    assert!(state.show_tool_activity);
    Ok(())
}

#[test]
fn terminal_tab_is_clickable_and_owns_raw_input_instead_of_the_chat_composer() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.terminal = TerminalFleetSnapshot {
        revision: 1,
        enabled: true,
        max_sessions: 6,
        sessions: Arc::from([TerminalSessionSnapshot {
            id: 7,
            title: "Terminal 7".to_owned(),
            cwd: PathBuf::from("workspace"),
            created_at: SystemTime::UNIX_EPOCH,
            status: TerminalStatus::Running,
            process_id: Some(42),
            output_revision: 2,
            frame: Arc::new(TerminalFrame {
                rows: 1,
                cols: 20,
                cursor_row: 0,
                cursor_col: 5,
                hide_cursor: false,
                application_cursor: false,
                bracketed_paste: true,
                alternate_screen: false,
                mouse_mode: decode::terminal::TerminalMouseMode::None,
                mouse_encoding: decode::terminal::TerminalMouseEncoding::Default,
                scrollback_offset: 0,
                content: Arc::from([TerminalRow {
                    spans: Arc::from([TerminalSpan {
                        text: "hello terminal".to_owned(),
                        style: TerminalStyle::default(),
                    }]),
                    wrapped: false,
                }]),
            }),
        }]),
        notice: None,
    };
    state.shell_ui.select_tab(ShellTab::Terminal);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 34)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains("Terminal 7"));
    assert!(rendered.contains("New"));
    assert!(rendered.contains("hello terminal"));

    handle_paste("raw\r\npaste", &mut state);
    handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );
    assert!(state.input_buffer.is_empty());
    assert!(!state.should_quit);
    assert!(command_rx.try_recv().is_err());
}

#[test]
fn submit_is_bound_to_current_snapshot_scope() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.conversation_epoch = 9;
    state.phase_revision = 17;
    state.input_buffer = "hello".to_owned();
    state.input_cursor = state.input_buffer.len();
    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    match command_rx.try_recv() {
        Ok(OrchestratorCommand::Submit {
            prompt,
            attachments,
            scope,
        }) => {
            assert_eq!(prompt, "hello");
            assert!(attachments.is_empty());
            assert_eq!(scope.conversation_epoch, 9);
            assert_eq!(scope.phase_revision, 17);
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn btw_side_question_can_be_sent_while_main_turn_is_streaming() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.conversation_epoch = 12;
    state.phase_revision = 34;
    state.deployment = "coding-model".to_owned();
    state.deployment_choices = Arc::from(["coding-model".to_owned(), "review-model".to_owned()]);
    state.input_buffer = "/btw Is the reconnect state machine bounded?".to_owned();
    state.input_cursor = state.input_buffer.len();

    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::AskSideQuestion {
            question,
            deployment,
            reasoning_effort,
            scope,
        }) => {
            assert_eq!(question, "Is the reconnect state machine bounded?");
            assert_eq!(deployment, "coding-model");
            assert_eq!(reasoning_effort, ReasoningEffort::Medium);
            assert_eq!(scope.conversation_epoch, 12);
            assert_eq!(scope.phase_revision, 34);
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing side-question command: {error}")),
    }
    assert!(state.input_buffer.is_empty());
    assert!(command_rx.try_recv().is_err());
    Ok(())
}

#[test]
fn btw_keeps_the_composer_text_when_no_deployment_is_available() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.input_buffer = "/btw keep this question".to_owned();
    state.input_cursor = state.input_buffer.len();

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert_eq!(state.input_buffer, "/btw keep this question");
    assert!(!state.side_chat_ui.is_open());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn invisible_side_question_is_not_submitted() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.deployment = "fast".to_owned();
    state.deployment_choices = Arc::from(["fast".to_owned()]);
    state.side_chat_ui.open(
        &state.side_chat,
        &state.deployment_choices,
        &state.deployment,
        state.reasoning_effort,
    );
    state.side_chat_ui.push_text("\u{200b}\u{2060}");

    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn side_question_model_stays_selected_when_choices_reorder() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.deployment = "fast".to_owned();
    state.deployment_choices = Arc::from(["fast".to_owned(), "deep".to_owned()]);
    state.side_chat_ui.open(
        &state.side_chat,
        &state.deployment_choices,
        &state.deployment,
        state.reasoning_effort,
    );
    state.side_chat_ui.select_model(1);
    state.deployment_choices = Arc::from(["deep".to_owned(), "fast".to_owned()]);
    state.side_chat_ui.push_text("Check the race");

    handle_key(
        KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::AskSideQuestion { deployment, .. }) => {
            assert_eq!(deployment, "deep");
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn busy_composer_opens_explicit_queue_or_steer_choice_with_tab_fallback() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(7);
    state.conversation_epoch = 4;
    state.phase_revision = 9;
    state.input_buffer = "Pivot to the API layer".to_owned();
    state.input_cursor = state.input_buffer.len();

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(state.follow_up_ui.is_open());
    assert!(state.input_buffer.is_empty());
    assert!(command_rx.try_recv().is_err());

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    match command_rx.try_recv() {
        Ok(OrchestratorCommand::EnqueueFollowUp { mode, text, scope }) => {
            assert_eq!(mode, FollowUpMode::Steer);
            assert_eq!(text, "Pivot to the API layer");
            assert_eq!(scope.conversation_epoch, 4);
            assert_eq!(scope.phase_revision, 9);
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing steer command: {error}")),
    }
    Ok(())
}

#[test]
fn invisible_follow_up_is_not_submitted_from_the_keyboard() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.follow_up_ui.open(&state.follow_ups, true);
    state.follow_up_ui.push_text("\u{200b}\u{2060}");

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn disabled_follow_up_actions_are_not_triggered_from_the_keyboard()
-> Result<(), Box<dyn std::error::Error>> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    let mut follow_ups = FollowUpState::default();
    follow_ups.enqueue(FollowUpMode::Queue, "queued".to_owned(), None)?;
    state.follow_ups = follow_ups.snapshot();
    state.phase = AgentPhase::Streaming;
    state.follow_up_ui.open(&state.follow_ups, false);
    for _ in 0..3 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let steer = follow_ups.enqueue(FollowUpMode::Steer, "steer".to_owned(), Some(7))?;
    follow_ups.mark_failed(steer.id, decode::notice::UiNotice::FollowUpInterrupted)?;
    state.follow_ups = follow_ups.snapshot();
    state.phase = AgentPhase::Idle;
    state.follow_up_ui.open(&state.follow_ups, false);
    state.follow_up_ui.select(1);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn disabled_github_actions_are_not_triggered_from_the_keyboard() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.github_ui.open(&state.github);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn checkout_confirmation_does_not_switch_to_a_replacement_pull_request() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.github = github_snapshot(&[7]);
    state.github_ui.open(&state.github);
    for _ in 0..3 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    state.github = github_snapshot(&[8]);
    state.github_ui.sync(&state.github);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn failed_github_confirmation_stays_open_for_retry() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.github = github_snapshot(&[]);
    state.github_ui.open(&state.github);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    for _ in 0..3 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert_eq!(
        state.github_ui.stage(),
        decode::ui::github::GitHubStage::ConfirmCreate
    );
}

fn github_snapshot(numbers: &[u64]) -> GitHubSnapshot {
    GitHubSnapshot {
        enabled: true,
        pull_requests: numbers
            .iter()
            .map(|number| PullRequestSummary {
                number: *number,
                title: format!("PR {number}"),
                state: "OPEN".to_owned(),
                url: format!("https://example.test/{number}"),
                head: format!("head-{number}"),
                base: "main".to_owned(),
                author: "author".to_owned(),
                draft: false,
            })
            .collect::<Vec<_>>()
            .into(),
        ..GitHubSnapshot::default()
    }
}

#[test]
fn agents_tab_tab_navigation_spawns_without_numeric_prompts() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    for character in "inspect parser".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SpawnSubagent {
            task, profile_id, ..
        }) => {
            assert_eq!(task, "inspect parser");
            assert_eq!(profile_id, "builtin:research");
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn agents_task_dialog_accepts_multiline_bracketed_paste() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_paste("inspect parser\r\nand tests", &mut state);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SpawnSubagent {
            task, profile_id, ..
        }) => {
            assert_eq!(task, "inspect parser\nand tests");
            assert_eq!(profile_id, "builtin:research");
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn agents_task_dialog_rejects_zero_width_only_text() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_paste("\u{200b}\u{200d}", &mut state);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(state.status_message.is_some());
}

#[test]
fn disabled_agent_actions_cannot_be_activated_from_the_keyboard() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.subagents = recovery_fleet(false);
    Arc::make_mut(&mut state.subagents.agents)[0].status = SubagentStatus::Completed;
    Arc::make_mut(&mut state.subagents.agents)[0].changed_files = Arc::from([]);
    state.agents_ui.sync(&state.subagents);
    state.shell_ui.select_tab(ShellTab::Agents);

    for _ in 0..4 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn agent_review_cannot_open_while_the_main_agent_is_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.subagents = recovery_fleet(false);
    state.agents_ui.sync(&state.subagents);
    state.shell_ui.select_tab(ShellTab::Agents);

    for _ in 0..5 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn agents_profile_picker_selects_writer_without_numeric_input() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_key(key(KeyCode::Down), &mut state, &command_tx);
    for character in "implement parser fix".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SpawnSubagent {
            task, profile_id, ..
        }) => {
            assert_eq!(task, "implement parser fix");
            assert_eq!(profile_id, "builtin:writer");
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn agents_dag_and_file_claims_use_tab_and_space_without_ids_or_paths() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.subagents = recovery_fleet(false);
    state.workspace_files = Arc::from([
        "src/parser.rs".to_owned(),
        "tests/parser_tests.rs".to_owned(),
    ]);
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_key(key(KeyCode::Down), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Char(' ')), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Char(' ')), &mut state, &command_tx);
    for character in "implement dependent parser fix".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SpawnSubagent {
            task,
            profile_id,
            dependencies,
            file_claims,
        }) => {
            assert_eq!(task, "implement dependent parser fix");
            assert_eq!(profile_id, "builtin:writer");
            assert_eq!(dependencies, vec![SubagentId::new(17)]);
            assert_eq!(file_claims, vec!["src/parser.rs"]);
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn agents_reload_profiles_is_reachable_by_tab() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.shell_ui.select_tab(ShellTab::Agents);

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::ReloadSubagentProfiles) => Ok(()),
        Ok(other) => Err(format!("unexpected command: {other:?}")),
        Err(error) => Err(format!("missing command: {error}")),
    }
}

#[test]
fn agents_recovery_resume_is_clickable_and_revision_bound() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.subagents = recovery_fleet(true);
    state.shell_ui.select_tab(ShellTab::Agents);
    let mut terminal = infallible(Terminal::new(TestBackend::new(110, 36)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.agents_ui.clicked(*column, row)
                    == Some(AgentHit::Browse(AgentBrowseFocus::Resume))
            })
            .map(|column| (column, row))
    });
    let (column, row) = point.ok_or_else(|| {
        std::io::Error::other("recoverable writer did not expose a Resume click region")
    })?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::ResumeSubagent {
            agent_id,
            expected_revision: 44,
        }) if agent_id == SubagentId::new(17)
    ));
    Ok(())
}

#[test]
fn agent_mouse_wheel_moves_the_visible_list_viewport() -> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut fleet = recovery_fleet(false);
    let template = fleet.agents[0].clone();
    fleet.agents = Arc::from(
        (1..=50)
            .map(|id| {
                let mut agent = template.clone();
                agent.id = SubagentId::new(id);
                agent.label = format!("delegated task {id}");
                agent
            })
            .collect::<Vec<_>>(),
    );
    let mut state = AppState::new();
    state.subagents = fleet;
    state.shell_ui.select_tab(ShellTab::Agents);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 36)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    for _ in 0..20 {
        handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            &mut state,
            &command_tx,
        );
    }
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    assert_eq!(state.agents_ui.selected_id(), Some(SubagentId::new(21)));
    assert!(terminal_text(&terminal).contains("agent-0021"));
    Ok(())
}

#[test]
fn agents_recovery_abandon_is_reachable_by_tab_and_revision_bound() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.subagents = recovery_fleet(true);
    state.agents_ui.sync(&state.subagents);
    state.shell_ui.select_tab(ShellTab::Agents);

    for _ in 0..7 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::AbandonSubagentRecovery {
            agent_id,
            expected_revision,
        }) => {
            assert_eq!(agent_id, SubagentId::new(17));
            assert_eq!(expected_revision, 44);
            Ok(())
        }
        Ok(other) => Err(format!("unexpected command: {other:?}")),
        Err(error) => Err(format!("missing command: {error}")),
    }
}

#[test]
fn agents_recovery_resume_has_no_click_region_without_recovered_worktree()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.subagents = recovery_fleet(false);
    state.shell_ui.select_tab(ShellTab::Agents);
    let mut terminal = infallible(Terminal::new(TestBackend::new(110, 36)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let resume_exists = (0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.agents_ui.clicked(column, row) == Some(AgentHit::Browse(AgentBrowseFocus::Resume))
        })
    });
    assert!(!resume_exists);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    for _ in 0..5 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn mcp_checkbox_click_emits_scoped_runtime_disable() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.mcp_servers = Arc::from([McpServerSnapshot {
        name: "files".to_owned(),
        transport: "STDIO",
        runtime_available: true,
        enabled: true,
        required: false,
        oauth: false,
        state: McpConnectionState::Connected,
        tool_count: 2,
        notice: decode::notice::UiNotice::McpToolsReady { count: 2 },
    }]);
    state.mcp_ui.open(1);
    let mut terminal = infallible(Terminal::new(TestBackend::new(110, 32)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.mcp_ui.clicked(*column, row) == Some(decode::ui::mcp::McpHit::Toggle)
            })
            .map(|column| (column, row))
    });
    let (column, row) = point.ok_or_else(|| std::io::Error::other("missing MCP toggle"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::McpSetEnabled {
            server,
            enabled: false,
            ..
        }) if server == "files"
    ));
    Ok(())
}

#[test]
fn disabled_mcp_actions_cannot_be_triggered_from_the_keyboard() {
    for (runtime_available, enabled, connection_state) in [
        (false, true, McpConnectionState::Disconnected),
        (true, false, McpConnectionState::Disconnected),
        (true, true, McpConnectionState::Connecting),
        (true, true, McpConnectionState::Reconnecting),
    ] {
        let (command_tx, mut command_rx) = mpsc::channel(1);
        let mut state = AppState::new();
        state.mcp_servers = Arc::from([McpServerSnapshot {
            name: "files".to_owned(),
            transport: "STDIO",
            runtime_available,
            enabled,
            required: false,
            oauth: false,
            state: connection_state,
            tool_count: 0,
            notice: decode::notice::UiNotice::Stopped,
        }]);
        state.mcp_ui.open(1);

        handle_key(key(KeyCode::Enter), &mut state, &command_tx);

        assert!(matches!(
            command_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}

#[test]
fn disabled_subagent_mcp_switch_cannot_be_triggered_from_the_keyboard() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.subagents.enabled = false;
    state.mcp_ui.open(0);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn mcp_mutations_stop_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.mcp_servers = Arc::from([McpServerSnapshot {
        name: "files".to_owned(),
        transport: "STDIO",
        runtime_available: true,
        enabled: true,
        required: false,
        oauth: false,
        state: McpConnectionState::Disconnected,
        tool_count: 0,
        notice: decode::notice::UiNotice::Stopped,
    }]);
    state.mcp_ui.open(1);
    state.phase = AgentPhase::Streaming;

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn disabled_mcp_oauth_cleanup_cannot_be_triggered_from_the_keyboard() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.mcp_servers = Arc::from([McpServerSnapshot {
        name: "oauth".to_owned(),
        transport: "HTTP",
        runtime_available: true,
        enabled: false,
        required: false,
        oauth: true,
        state: McpConnectionState::Disconnected,
        tool_count: 0,
        notice: decode::notice::UiNotice::Stopped,
    }]);
    state.mcp_ui.open(1);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn lsp_shortcut_and_checkbox_are_interactive_and_scoped() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(3);
    let mut state = AppState::new();
    state.conversation_epoch = 9;
    state.lsp_servers = Arc::from([LspServerSnapshot {
        name: "rust-analyzer".to_owned(),
        language_id: "rust".to_owned(),
        runtime_available: true,
        enabled: true,
        required: false,
        auto_start: true,
        detected: true,
        state: LspConnectionState::Connected,
        diagnostic_count: 0,
        notice: decode::notice::UiNotice::LspReady,
    }]);
    handle_key(
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );
    assert!(state.lsp_ui.is_open());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::LspRefresh { scope }) if scope.conversation_epoch == 9
    ));

    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 36)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.lsp_ui.clicked(*column, row) == Some(decode::ui::lsp::LspHit::Toggle)
            })
            .map(|column| (column, row))
    });
    let (column, row) = point.ok_or_else(|| std::io::Error::other("missing LSP toggle"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::LspSetEnabled {
            server,
            enabled: false,
            scope,
        }) if server == "rust-analyzer" && scope.conversation_epoch == 9
    ));
    Ok(())
}

#[test]
fn lsp_space_does_not_toggle_from_an_unrelated_control() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = lsp_ui_state();
    state.lsp_ui.focus(LspFocus::Close);

    handle_key(key(KeyCode::Char(' ')), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(state.lsp_ui.is_open());
}

#[test]
fn lsp_actions_hidden_on_the_other_pane_are_ignored() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = lsp_ui_state();
    state.lsp_ui.set_pane(LspPane::Diagnostics);
    state.lsp_ui.focus(LspFocus::Primary);

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    state.lsp_ui.focus(LspFocus::Add);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.lsp_ui.is_editing());

    state.lsp_ui.set_pane(LspPane::Servers);
    state.lsp_ui.focus(LspFocus::Mention);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(state.input_buffer.is_empty());
    assert!(state.lsp_ui.is_open());
}

#[test]
fn lsp_mutations_stop_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = lsp_ui_state();
    state.phase = AgentPhase::Streaming;
    state.lsp_ui.focus(LspFocus::Toggle);

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

fn lsp_ui_state() -> AppState {
    let mut state = AppState::new();
    state.lsp_servers = Arc::from([LspServerSnapshot {
        name: "rust-analyzer".to_owned(),
        language_id: "rust".to_owned(),
        runtime_available: true,
        enabled: true,
        required: false,
        auto_start: true,
        detected: true,
        state: LspConnectionState::Connected,
        diagnostic_count: 1,
        notice: decode::notice::UiNotice::LspReady,
    }]);
    state.lsp_diagnostics = Arc::from([LspDiagnostic {
        server: "rust-analyzer".to_owned(),
        path: "src/main.rs".to_owned(),
        line: 1,
        column: 1,
        end_line: 1,
        end_column: 2,
        severity: LspDiagnosticSeverity::Error,
        message: "broken".to_owned(),
        source: Some("rustc".to_owned()),
        code: Some("E0001".to_owned()),
    }]);
    state
        .lsp_ui
        .open(state.lsp_servers.len(), state.lsp_diagnostics.len());
    state
}

#[test]
fn repository_index_shortcut_and_search_button_are_scoped() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(3);
    let mut state = AppState::new();
    state.conversation_epoch = 12;
    let mut snapshot = CodeIndexSnapshot::new(true);
    snapshot.state = CodeIndexState::Ready;
    snapshot.indexed_files = 20;
    snapshot.chunk_count = 40;
    state.code_index = snapshot;
    handle_key(
        KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );
    assert!(state.code_index_ui.is_open());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::CodeIndexPoll { scope }) if scope.conversation_epoch == 12
    ));
    state.code_index_ui.push_text("authentication flow");

    let mut terminal = infallible(Terminal::new(TestBackend::new(130, 40)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.code_index_ui.clicked(*column, row)
                    == Some(decode::ui::code_index::CodeIndexHitRegion::Search)
            })
            .map(|column| (column, row))
    });
    let (column, row) =
        point.ok_or_else(|| std::io::Error::other("missing repository-index search button"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::CodeIndexSearch {
            query,
            path: None,
            top: 12,
            scope,
        }) if query == "authentication flow" && scope.conversation_epoch == 12
    ));
    Ok(())
}

#[test]
fn repository_index_rejects_an_invisible_keyboard_query() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    let mut snapshot = CodeIndexSnapshot::new(true);
    snapshot.state = CodeIndexState::Ready;
    state.code_index = snapshot;
    state.code_index_ui.open(0);
    state.code_index_ui.push_text("\u{200b}\u{200d}");

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn modes_checkbox_click_emits_scoped_plan_toggle() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.conversation_epoch = 4;
    state.phase_revision = 9;
    state.modes_ui.open(&WorkModes::default());
    let mut terminal = infallible(Terminal::new(TestBackend::new(110, 34)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.modes_ui.clicked(*column, row) == Some(decode::ui::modes::ModesHit::Plan)
            })
            .map(|column| (column, row))
    });
    let (column, row) = point.ok_or_else(|| std::io::Error::other("missing Plan toggle"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetPlanMode {
            enabled: true,
            scope
        }) if scope.conversation_epoch == 4 && scope.phase_revision == 9
    ));
    Ok(())
}

#[test]
fn instruction_source_checkbox_click_emits_scoped_toggle() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.conversation_epoch = 6;
    state.phase_revision = 12;
    state.instructions = InstructionSetSnapshot {
        revision: 1,
        project_enabled: true,
        active_project_bytes: 20,
        sources: Arc::from([
            InstructionSourceSnapshot {
                id: "system".to_owned(),
                display_path: "trusted.md".to_owned(),
                scope: "all requests".to_owned(),
                origin: InstructionOrigin::System,
                bytes: 10,
                include_count: 0,
                enabled: true,
                locked: true,
            },
            InstructionSourceSnapshot {
                id: "project:frontend/AGENTS.md".to_owned(),
                display_path: "frontend/AGENTS.md".to_owned(),
                scope: "frontend".to_owned(),
                origin: InstructionOrigin::Project,
                bytes: 20,
                include_count: 0,
                enabled: true,
                locked: false,
            },
        ]),
        warnings: Arc::from([]),
    };
    state.instructions_ui.open(state.instructions.sources.len());
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.instructions_ui.clicked(*column, row)
                    == Some(decode::ui::instructions::InstructionsHit::Source(1))
            })
            .map(|column| (column, row))
    });
    let (column, row) =
        point.ok_or_else(|| std::io::Error::other("missing instruction-source toggle"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetInstructionSourceEnabled {
            id,
            enabled: false,
            scope
        }) if id == "project:frontend/AGENTS.md"
            && scope.conversation_epoch == 6
            && scope.phase_revision == 12
    ));
    Ok(())
}

#[test]
fn instruction_controls_stop_mutating_after_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.instructions.project_enabled = true;
    state.instructions_ui.open(state.instructions.sources.len());
    state.phase = AgentPhase::Streaming;

    handle_key(key(KeyCode::BackTab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn skill_controls_stop_mutating_after_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.skills = SkillCatalogSnapshot {
        skills: Arc::from([SkillSummary {
            id: "project:review".to_owned(),
            name: "Review".to_owned(),
            description: String::new(),
            source: SkillSource::Project,
            display_path: ".decode/skills/review/SKILL.md".to_owned(),
            enabled: true,
            resource_count: 0,
        }]),
        ..SkillCatalogSnapshot::default()
    };
    state.skills_ui.open(state.skills.skills.len());
    state.skills_ui.sync(&state.skills);
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            matches!(
                state.skills_ui.clicked(column, row),
                Some(
                    decode::ui::skills::SkillsHit::Skill(_) | decode::ui::skills::SkillsHit::Reload
                )
            )
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn automation_hook_checkbox_click_emits_scoped_toggle() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.conversation_epoch = 8;
    state.phase_revision = 14;
    state.automation = AutomationSnapshot {
        revision: 2,
        user_commands_dir: Some(PathBuf::from("user/commands")),
        project_commands_dir: PathBuf::from("project/commands"),
        user_hooks_dir: Some(PathBuf::from("user/hooks")),
        commands: Arc::from([]),
        hooks: Arc::from([HookSummary {
            id: "guard".to_owned(),
            name: "Guard".to_owned(),
            description: "Checks writes".to_owned(),
            source_path: PathBuf::from("guard.toml"),
            event: HookEvent::PreToolUse,
            program: PathBuf::from(r"C:\tools\guard.exe"),
            args: Arc::from([]),
            timeout: Duration::from_secs(2),
            blocking: true,
            enabled: true,
            tool_match: Arc::from(["write_file".to_owned()]),
        }]),
        diagnostics: Arc::from([]),
    };
    state.automation_ui.open(&state.automation);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.automation_ui.clicked(*column, row)
                    == Some(decode::ui::automation::AutomationHit::ToggleHook(0))
            })
            .map(|column| (column, row))
    });
    let (column, row) =
        point.ok_or_else(|| std::io::Error::other("missing automation hook toggle"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetHookEnabled {
            id,
            enabled: false,
            scope,
        }) if id == "guard"
            && scope.conversation_epoch == 8
            && scope.phase_revision == 14
    ));
    Ok(())
}

#[test]
fn custom_slash_command_is_searchable_and_inserted_without_auto_submit() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.automation.commands = Arc::from([CustomCommandSummary {
        id: "review".to_owned(),
        name: "Security review".to_owned(),
        description: "Inspect one path".to_owned(),
        source: AutomationSource::Project,
        source_path: PathBuf::from("review.toml"),
        argument_hint: "<path>".to_owned(),
        requires_arguments: true,
    }]);

    handle_key(key(KeyCode::Char('/')), &mut state, &command_tx);
    for character in "security".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert_eq!(state.input_buffer, "/review ");
    assert!(!state.palette_ui.is_open());
    assert!(command_rx.try_recv().is_err());
}

#[test]
fn palette_accepts_bracketed_paste_as_query_text() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    handle_key(key(KeyCode::Char('/')), &mut state, &command_tx);

    handle_paste("rewind\r\nnow", &mut state);

    assert_eq!(state.palette_ui.query(), "rewind now");
}

#[test]
fn plugin_source_paste_appends_to_existing_input() {
    let mut state = AppState::new();
    state.plugin_ui.open(&state.plugins);
    state
        .plugin_ui
        .focus(decode::ui::plugins::PluginFocus::Input);
    state.plugin_ui.set_input("https://");

    handle_paste("example.test/index.json", &mut state);

    assert_eq!(state.plugin_ui.input(), "https://example.test/index.json");
}

#[test]
fn invisible_plugin_source_is_not_submitted() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.plugin_ui.open(&state.plugins);
    state.plugin_ui.set_input("\u{200b}\u{200d}");
    state
        .plugin_ui
        .focus(decode::ui::plugins::PluginFocus::AddSource);

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn plugin_mutations_stop_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.plugin_ui.open(&state.plugins);
    state.plugin_ui.set_input("https://example.test/index.json");
    state
        .plugin_ui
        .focus(decode::ui::plugins::PluginFocus::AddSource);
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(130, 44)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;

    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.plugin_ui.clicked(column, row) == Some(decode::ui::plugins::PluginHit::AddSource)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn plan_explore_review_goal_and_deep_toggle_independently_through_keyboard_ui()
-> Result<(), Box<dyn std::error::Error>> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.modes_ui.open(&WorkModes::default());

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetPlanMode { enabled: true, .. })
    ));
    state.work_modes.plan = true;

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetExploreMode { enabled: true, .. })
    ));
    state.work_modes.explore = true;

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetReviewMode { enabled: true, .. })
    ));
    state.work_modes.review = true;

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_paste("Finish the combined workflow", &mut state);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetGoal {
            objective: Some(objective),
            ..
        }) if objective == "Finish the combined workflow"
    ));
    state
        .work_modes
        .set_goal(Some("Finish the combined workflow".to_owned()))?;

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetDeepThinkingMode { enabled: true, .. })
    ));
    assert!(state.work_modes.plan);
    assert!(state.work_modes.explore);
    assert!(state.work_modes.review);
    assert!(state.work_modes.goal_enabled());
    Ok(())
}

#[test]
fn goal_editor_is_reachable_with_tab_and_accepts_multiline_paste() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.modes_ui.open(&WorkModes::default());
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_paste("Ship parser\r\nwithout regressions", &mut state);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SetGoal {
            objective: Some(objective),
            ..
        }) => assert_eq!(objective, "Ship parser\nwithout regressions"),
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }
    Ok(())
}

#[test]
fn invisible_goal_is_not_submitted() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.modes_ui.open(&WorkModes::default());
    for _ in 0..3 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_paste("\u{200b}\u{200d}", &mut state);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn modes_controls_stop_mutating_after_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.modes_ui.open(&WorkModes::default());
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(110, 34)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;

    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.modes_ui.clicked(column, row) == Some(decode::ui::modes::ModesHit::Plan)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn plan_review_can_be_clicked_edited_and_approved_without_numeric_input()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    let review = Arc::new(PlanReview {
        turn_id: 11,
        review_id: 7,
        plan: "1. Inspect\n2. Test".to_owned(),
        deployment: "coding-prod".to_owned(),
        reasoning_effort: ReasoningEffort::XHigh,
        reasoning_mode: Some(ReasoningMode::Pro),
    });
    state.apply_snapshot(&UiSnapshot {
        phase: AgentPhase::AwaitingPlanApproval,
        phase_revision: 1,
        history_revision: 1,
        active_turn_id: Some(11),
        modal: Some(UiModal::PlanApproval { review }),
        ..UiSnapshot::default()
    });
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let point = (0..area.height).find_map(|row| {
        (0..area.width)
            .find(|column| {
                state.plan_approval_ui.clicked(*column, row)
                    == Some(decode::ui::modes::PlanHit::Text)
            })
            .map(|column| (column, row))
    });
    let (column, row) = point.ok_or_else(|| std::io::Error::other("missing plan editor"))?;
    handle_mouse(left_click(column, row), &mut state, &command_tx);
    handle_paste("\n3. Verify", &mut state);
    handle_key(key(KeyCode::Esc), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::DecidePlan {
            turn_id: 11,
            review_id: 7,
            decision: decode::agent::PlanDecision::Approve { plan }
        }) if plan.ends_with("3. Verify")
    ));
    Ok(())
}

#[test]
fn invisible_edited_plan_cannot_be_approved() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    let review = Arc::new(PlanReview {
        turn_id: 11,
        review_id: 7,
        plan: "x".to_owned(),
        deployment: "coding-prod".to_owned(),
        reasoning_effort: ReasoningEffort::XHigh,
        reasoning_mode: None,
    });
    state.apply_snapshot(&UiSnapshot {
        phase: AgentPhase::AwaitingPlanApproval,
        phase_revision: 1,
        history_revision: 1,
        active_turn_id: Some(11),
        modal: Some(UiModal::PlanApproval { review }),
        ..UiSnapshot::default()
    });
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    handle_key(key(KeyCode::Backspace), &mut state, &command_tx);
    handle_paste("\u{200b}\u{200d}", &mut state);
    handle_key(key(KeyCode::Esc), &mut state, &command_tx);

    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.plan_approval_ui.clicked(column, row) == Some(decode::ui::modes::PlanHit::Approve)
        })
    }));

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn stale_snapshots_cannot_regress_epoch_or_revisions() {
    let mut state = AppState::new();
    let fresh = UiSnapshot {
        conversation_epoch: 3,
        phase_revision: 5,
        history_revision: 7,
        phase: AgentPhase::Streaming,
        status: "fresh".to_owned(),
        ..UiSnapshot::default()
    };
    state.apply_snapshot(&fresh);

    let mut stale_epoch = fresh.clone();
    stale_epoch.conversation_epoch = 2;
    stale_epoch.phase_revision = 99;
    stale_epoch.history_revision = 99;
    stale_epoch.status = "stale epoch".to_owned();
    state.apply_snapshot(&stale_epoch);
    assert_eq!(state.conversation_epoch, 3);
    assert_eq!(
        state.status_message.as_deref(),
        Some("writing the response")
    );

    let mut stale_phase = fresh.clone();
    stale_phase.phase_revision = 4;
    stale_phase.history_revision = 8;
    stale_phase.status = "stale phase".to_owned();
    state.apply_snapshot(&stale_phase);
    assert_eq!(state.phase_revision, 5);
    assert_eq!(state.history_revision, 7);
    assert_eq!(
        state.status_message.as_deref(),
        Some("writing the response")
    );

    let mut stale_history = fresh;
    stale_history.phase_revision = 6;
    stale_history.history_revision = 6;
    stale_history.status = "stale history".to_owned();
    state.apply_snapshot(&stale_history);
    assert_eq!(state.phase_revision, 5);
    assert_eq!(state.history_revision, 7);
    assert_eq!(
        state.status_message.as_deref(),
        Some("writing the response")
    );
}

#[test]
fn snapshot_tool_actions_replace_and_prune_diagnostic_cache() {
    let mut state = AppState::new();
    Arc::make_mut(&mut state.tool_actions).insert(
        1,
        ToolAction::ReadFile {
            path: "stale".to_owned(),
        },
    );
    let mut actions = BTreeMap::new();
    actions.insert(
        2,
        ToolAction::ReadFile {
            path: "current".to_owned(),
        },
    );
    let snapshot = UiSnapshot {
        phase_revision: 1,
        history_revision: 1,
        tool_actions: Arc::new(actions),
        ..UiSnapshot::default()
    };
    state.apply_snapshot(&snapshot);

    assert!(!state.tool_actions.contains_key(&1));
    assert!(state.tool_actions.contains_key(&2));
}

#[test]
fn stale_epoch_diagnostics_do_not_mutate_current_ui_state() {
    use decode::agent::orchestrator::OrchestratorEvent;

    let mut state = AppState::new();
    state.conversation_epoch = 8;
    state.active_turn_id = Some(3);
    state.status_message = Some("current".to_owned());
    let action = ToolAction::ReadFile {
        path: "stale".to_owned(),
    };
    state.handle_orchestrator_event(OrchestratorEvent::ToolStarted {
        conversation_epoch: 7,
        turn_id: 3,
        action_id: 4,
        action: action.clone(),
    });
    state.handle_orchestrator_event(OrchestratorEvent::ToolCompleted {
        conversation_epoch: 7,
        turn_id: 3,
        action_id: 4,
        action,
        outcome: ToolOutcome::success("stale"),
    });
    state.handle_orchestrator_event(OrchestratorEvent::RetryScheduled {
        conversation_epoch: 7,
        turn_id: 3,
        next_attempt: 2,
        max_attempts: 3,
        reason: "stale".to_owned(),
    });
    state.handle_orchestrator_event(OrchestratorEvent::WhipAcknowledged {
        conversation_epoch: 7,
        turn_id: 3,
        kind: WhipKind::Soft,
    });

    assert!(state.tool_actions.is_empty());
    assert!(state.retry.is_none());
    assert_eq!(state.whip.acknowledgements, 0);
    assert_eq!(state.status_message.as_deref(), Some("current"));
}

#[test]
fn same_epoch_diagnostics_are_ignored_after_the_turn_is_idle() {
    use decode::agent::orchestrator::OrchestratorEvent;

    let mut state = AppState::new();
    state.conversation_epoch = 8;
    state.active_turn_id = None;
    state.status_message = Some("ready".to_owned());
    state.handle_orchestrator_event(OrchestratorEvent::ToolStarted {
        conversation_epoch: 8,
        turn_id: 3,
        action_id: 4,
        action: ToolAction::ReadFile {
            path: "stale".to_owned(),
        },
    });

    assert!(state.tool_actions.is_empty());
    assert_eq!(state.status_message.as_deref(), Some("ready"));
}

#[test]
fn modal_ctrl_controls_use_urgent_plane_while_escape_is_local() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let urgent = UrgentControlHandle::default();
    let mut state = confirmation_state("echo safe");

    handle_key_with_control(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Interrupting turn #11")
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    handle_key_with_control(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert_eq!(state.status_message.as_deref(), Some("Reset requested"));
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    handle_key_with_control(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert!(state.pending_confirmation.is_none());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::Decline,
            ..
        })
    ));
}

#[test]
fn plan_review_does_not_swallow_urgent_controls() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let urgent = UrgentControlHandle::default();
    let mut state = AppState::new();
    let review = Arc::new(PlanReview {
        turn_id: 23,
        review_id: 5,
        plan: "1. Inspect\n2. Verify".to_owned(),
        deployment: "coding-model".to_owned(),
        reasoning_effort: ReasoningEffort::High,
        reasoning_mode: None,
    });
    state.apply_snapshot(&UiSnapshot {
        phase: AgentPhase::AwaitingPlanApproval,
        active_turn_id: Some(23),
        modal: Some(UiModal::PlanApproval { review }),
        ..UiSnapshot::default()
    });

    handle_key_with_control(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Interrupting turn #23")
    );
    assert!(state.pending_plan_review.is_some());

    handle_key_with_control(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert_eq!(state.status_message.as_deref(), Some("Reset requested"));
    assert!(state.pending_plan_review.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn continuation_modal_ctrl_c_interrupts_and_escape_stops_locally() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let urgent = UrgentControlHandle::default();
    let mut state = AppState::new();
    state.phase = AgentPhase::AwaitingContinuation;
    state.active_turn_id = Some(7);
    state.pending_continuation = Some(PendingContinuation {
        turn_id: 7,
        continuation_id: 1,
        completed_iterations: 8,
        max_iterations: 8,
    });

    handle_key_with_control(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert_eq!(
        state.status_message.as_deref(),
        Some("Interrupting turn #7")
    );
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    handle_key_with_control(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::ContinueToolLoop {
            continue_loop: false,
            ..
        })
    ));
}

#[test]
fn cjk_width_and_zwj_clusters_move_as_editor_units() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.input_buffer = "界👩‍💻x".to_owned();
    state.input_cursor = state.input_buffer.len();
    handle_key(key(KeyCode::Left), &mut state, &command_tx);
    handle_key(key(KeyCode::Left), &mut state, &command_tx);
    assert_eq!(state.input_cursor, "界".len());
    handle_key(key(KeyCode::Right), &mut state, &command_tx);
    handle_key(key(KeyCode::Backspace), &mut state, &command_tx);
    assert_eq!(state.input_buffer, "界x");
    assert_eq!(state.input_cursor, "界".len());

    state.input_buffer = "界x\nabcd".to_owned();
    state.input_cursor = "界".len();
    handle_key(key(KeyCode::Down), &mut state, &command_tx);
    assert_eq!(state.input_cursor, "界x\nab".len());
}

#[test]
fn confirmation_modal_sends_bound_one_shot_decision() -> Result<(), String> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.phase = AgentPhase::AwaitingConfirmation;
    state.active_turn_id = Some(11);
    state.pending_confirmation = Some(PendingConfirmation {
        turn_id: 11,
        action_id: 29,
        action: ToolAction::ExecuteCommand {
            command: "cargo test".to_owned(),
            requires_confirmation: false,
        },
        command: "cargo test".to_owned(),
        command_bytes: "cargo test".len(),
        command_digest: CommandDigest::for_command("cargo test"),
        model_requested: false,
        reason: ConfirmationReason::PolicyRequired,
        session_trust_available: true,
    });
    let backend = TestBackend::new(100, 30);
    let mut terminal = Terminal::new(backend).map_err(|error| error.to_string())?;
    terminal
        .draw(|frame| render::draw(frame, &mut state))
        .map_err(|error| error.to_string())?;

    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );

    assert!(state.pending_confirmation.is_none());
    match command_rx.try_recv() {
        Ok(OrchestratorCommand::Confirm {
            turn_id,
            action_id,
            decision,
        }) => {
            assert_eq!(turn_id, 11);
            assert_eq!(action_id, 29);
            assert_eq!(decision, ShellApprovalDecision::RunOnce);
        }
        Ok(other) => return Err(format!("unexpected command: {other:?}")),
        Err(error) => return Err(format!("missing command: {error}")),
    }

    Ok(())
}

#[test]
fn long_confirmation_requires_viewing_the_suffix() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    let command = "x".repeat(1_300);
    state.phase = AgentPhase::AwaitingConfirmation;
    state.active_turn_id = Some(1);
    state.pending_confirmation = Some(PendingConfirmation {
        turn_id: 1,
        action_id: 2,
        action: ToolAction::ExecuteCommand {
            command: command.clone(),
            requires_confirmation: true,
        },
        command_bytes: command.len(),
        command_digest: CommandDigest::for_command(&command),
        command,
        model_requested: true,
        reason: ConfirmationReason::ModelRequested,
        session_trust_available: false,
    });

    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    handle_key(
        KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let backend = TestBackend::new(100, 30);
    let mut terminal = infallible(Terminal::new(backend));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    for _ in 0..20 {
        handle_key(key(KeyCode::PageDown), &mut state, &command_tx);
    }
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    handle_key(key(KeyCode::End), &mut state, &command_tx);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::RunOnce,
            ..
        })
    ));
    Ok(())
}

#[test]
fn scrollable_command_below_old_length_threshold_still_requires_end_review()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let command = "abcdef ".repeat(100);
    assert!(command.len() < 1_200);
    let mut state = confirmation_state(&command);
    let backend = TestBackend::new(54, 24);
    let mut terminal = infallible(Terminal::new(backend));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(state.confirmation_view_ready);
    assert!(state.confirmation_max_scroll > 0);

    handle_key(key(KeyCode::Char('y')), &mut state, &command_tx);
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    for _ in 0..100 {
        handle_key(key(KeyCode::PageDown), &mut state, &command_tx);
    }
    assert_eq!(state.confirmation_scroll, state.confirmation_max_scroll);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(!state.confirmation_suffix_viewed);
    handle_key(key(KeyCode::Char('y')), &mut state, &command_tx);
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    handle_key(key(KeyCode::End), &mut state, &command_tx);
    handle_key(key(KeyCode::Char('y')), &mut state, &command_tx);
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(state.confirmation_suffix_viewed);
    handle_key(key(KeyCode::Char('y')), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::RunOnce,
            ..
        })
    ));
    Ok(())
}

#[test]
fn continuation_modal_blocks_input_and_sends_stop() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.phase = AgentPhase::AwaitingContinuation;
    state.active_turn_id = Some(7);
    state.pending_continuation = Some(PendingContinuation {
        turn_id: 7,
        continuation_id: 1,
        completed_iterations: 8,
        max_iterations: 8,
    });

    handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );

    assert!(state.pending_continuation.is_none());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::ContinueToolLoop {
            turn_id: 7,
            continue_loop: false,
            ..
        })
    ));
}

#[test]
fn whip_mouse_click_requires_active_phase_and_exact_hitbox() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(41);
    state.whip_hitbox = Some(Rect::new(70, 3, 8, 3));

    handle_mouse(left_click(69, 4), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    handle_mouse(left_click(70, 3), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Whip { turn_id: 41 })
    ));

    state.phase = AgentPhase::Idle;
    handle_mouse(left_click(72, 4), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn disabled_mouse_ignores_clicks_even_inside_active_hitbox() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let urgent = UrgentControlHandle::default();
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(41);
    state.whip_hitbox = Some(Rect::new(70, 3, 8, 3));

    handle_mouse_enabled(left_click(72, 4), &mut state, &command_tx, &urgent, false);
    assert_eq!(state.whip.requests_sent, 0);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn pixel_remains_visible_but_cannot_be_fed_while_the_agent_is_busy() -> Result<(), std::io::Error> {
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(9);
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 9,
        kind: HistoryKind::User,
        content: "keep working".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Pending,
        approx_tokens: 3,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;
    let has_feed_action = (0..area.height).any(|row| {
        (0..area.width).any(|column| state.shell_ui.hit(column, row) == Some(ShellHit::MascotFeed))
    });
    assert!(!has_feed_action);
    assert!(terminal_text(&terminal).contains(".---------------."));

    let (command_tx, _command_rx) = mpsc::channel(1);
    handle_key(
        KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.status_message.is_none());
    Ok(())
}

#[test]
fn pixel_remains_visible_above_an_idle_existing_conversation() -> Result<(), std::io::Error> {
    let mut state = AppState::new();
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 1,
        kind: HistoryKind::Assistant,
        content: "finished response".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 3,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    assert!(terminal_text(&terminal).contains(".---------------."));
    Ok(())
}

#[test]
fn disabled_pixel_is_absent_from_an_existing_conversation() -> Result<(), std::io::Error> {
    let mut state = AppState::new();
    state.mascot.set_enabled(false);
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 1,
        kind: HistoryKind::Assistant,
        content: "finished response".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 3,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    assert!(!terminal_text(&terminal).contains(".---------------."));
    Ok(())
}

#[test]
fn reenabled_pixel_keeps_feed_reaction_color_above_a_long_conversation()
-> Result<(), std::io::Error> {
    let mut state = AppState::new();
    state.history = Arc::from(
        (1..=128)
            .map(|sequence| HistoryEntry {
                epoch: 1,
                revision: sequence,
                sequence,
                turn_id: sequence,
                kind: HistoryKind::Assistant,
                content: format!("finished response {sequence}"),
                attachments: Vec::new(),
                status: HistoryStatus::Committed,
                approx_tokens: 3,
                created_at: Utc::now(),
                api_items: Vec::new(),
                tool_summary: None,
                turn_metrics: None,
            })
            .collect::<Vec<_>>(),
    );
    state.mascot.set_enabled(false);
    state.mascot.set_enabled(true);
    state.mascot.feed(Instant::now());
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let buffer = terminal.backend().buffer();
    let (column, row) = find_ascii_in_buffer(buffer, "shipped!")
        .ok_or_else(|| std::io::Error::other("feed reaction was not rendered"))?;
    assert_eq!(buffer[(column, row)].fg, Color::LightGreen);
    Ok(())
}

#[test]
fn compacted_tool_result_summary_is_shown_without_an_ellipsis() {
    let mut state = AppState::new();
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 1,
        kind: HistoryKind::ToolResult {
            action_id: 52,
            tool_name: "code_index_status".to_owned(),
            outcome: ToolResultStatus::Success,
        },
        content: "[tool result compacted: tool=code_index_status, action_id=52, path=-, bytes=598, sha256=01391e3a4426e7dacf39b6689452; rerun the tool if exact bytes are needed]".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 20,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 40)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let rendered = terminal_text(&terminal);
    assert!(rendered.contains("needed]"));
}

#[test]
fn context_compaction_is_identified_as_a_context_event() {
    let mut state = AppState::new();
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 0,
        kind: HistoryKind::Assistant,
        content: "[17 older history entries compacted into deterministic API-context summaries]"
            .to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Superseded,
        approx_tokens: 16,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(100, 20)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let rendered = terminal_text(&terminal);
    assert!(rendered.contains("Context compacted"));
    assert!(rendered.contains("17"));
    assert!(!rendered.contains("Assistant | superseded"));
}

#[test]
fn completed_answer_has_distinct_color_and_exact_turn_metrics()
-> Result<(), Box<dyn std::error::Error>> {
    let mut state = AppState::new();
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 7,
        kind: HistoryKind::Assistant,
        content: "final-result-marker".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 4,
        created_at: Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: Some(TurnMetrics {
            elapsed_millis: 66_250,
            input_tokens: 1_000,
            output_tokens: 250,
            total_tokens: 1_250,
            cost_microusd: Some(10_880),
        }),
    }]);
    let mut terminal = infallible(Terminal::new(TestBackend::new(100, 20)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let rendered = terminal_text(&terminal);
    assert!(rendered.contains("Elapsed: 1:06"));
    assert!(rendered.contains("Tokens: 1250 (1000/250)"));
    assert!(rendered.contains("Cost: $0.010880"));
    assert!(!rendered.contains("Assistant"));
    assert!(rendered.contains("DEcode") || rendered.contains("Pixel"));
    let (column, row) = find_ascii_in_buffer(terminal.backend().buffer(), "final-result-marker")
        .ok_or_else(|| std::io::Error::other("final answer was not rendered"))?;
    assert!(row > 1);
    assert_eq!(
        terminal.backend().buffer()[(column, row - 1)].fg,
        ratatui::style::Color::Gray
    );
    assert_eq!(
        terminal.backend().buffer()[(column, row - 2)].fg,
        ratatui::style::Color::DarkGray
    );
    assert_eq!(
        terminal.backend().buffer()[(column, row)].fg,
        ratatui::style::Color::LightCyan
    );
    Ok(())
}

#[test]
fn large_usage_counts_fit_in_the_narrow_status_panel() {
    let mut state = AppState::new();
    state.tokens_input = 3_103_195;
    state.tokens_output = 130_874;
    state.tokens_total = 3_234_069;
    state.context_budget = 120_000;
    state.usage.last_response_tokens = Some(109_000);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 50)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let rendered = terminal_text(&terminal);
    assert!(rendered.contains("3.1M/131K/3.2M"));
    assert!(rendered.contains("120K/109K"));
}

#[test]
fn apply_patch_diff_expands_and_collapses_on_enter() -> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.history = Arc::from([HistoryEntry {
        epoch: 1,
        revision: 1,
        sequence: 1,
        turn_id: 1,
        kind: HistoryKind::ToolResult {
            action_id: 9,
            tool_name: "apply_patch".to_owned(),
            outcome: ToolResultStatus::Success,
        },
        content: "done".to_owned(),
        attachments: Vec::new(),
        status: HistoryStatus::Committed,
        approx_tokens: 1,
        created_at: chrono::Utc::now(),
        api_items: Vec::new(),
        tool_summary: None,
        turn_metrics: None,
    }]);
    Arc::make_mut(&mut state.tool_actions).insert(
        9,
        ToolAction::ApplyPatch {
            path: "src/lib.rs".to_owned(),
            search: "old\n".to_owned(),
            replace: "new\n".to_owned(),
        },
    );
    state.selected_tool = Some(9);
    let backend = TestBackend::new(100, 30);
    let mut terminal = infallible(Terminal::new(backend));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(!terminal_text(&terminal).contains("-old"));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let expanded = terminal_text(&terminal);
    assert!(expanded.contains("-old"));
    assert!(expanded.contains("+new"));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(!terminal_text(&terminal).contains("-old"));
    Ok(())
}

#[test]
fn render_stores_the_whip_widgets_real_rectangle() -> Result<(), std::io::Error> {
    let backend = TestBackend::new(100, 30);
    let mut terminal = infallible(Terminal::new(backend));
    let mut state = AppState::new();
    state.phase = AgentPhase::Requesting;
    state.active_turn_id = Some(1);

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let hitbox = state
        .whip_hitbox
        .ok_or_else(|| std::io::Error::other("render did not store whip hitbox"))?;
    assert!(hitbox.width > 0);
    assert!(hitbox.height > 0);
    assert!(rect_contains(hitbox, hitbox.x, hitbox.y));
    assert!(!rect_contains(hitbox, hitbox.right(), hitbox.y));
    Ok(())
}

#[test]
fn tiny_terminal_render_is_safe() -> Result<(), std::io::Error> {
    let backend = TestBackend::new(10, 3);
    let mut terminal = infallible(Terminal::new(backend));
    let mut state = AppState::new();
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(state.whip_hitbox.is_none());
    Ok(())
}

#[test]
fn usage_dashboard_opens_by_mouse_and_closes_with_tab_fallback() -> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.usage = UsageSnapshot {
        usage: TokenUsage {
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 30,
            total_tokens: 150,
        },
        last_response_tokens: Some(150),
        estimated_cost_microusd: 42_000,
        has_unpriced_usage: false,
        pricing_configured: true,
        deployments: Arc::from([DeploymentUsageSnapshot {
            deployment: "coding-prod".to_owned(),
            usage: TokenUsage {
                input_tokens: 120,
                cached_input_tokens: 20,
                output_tokens: 30,
                total_tokens: 150,
            },
            cost_microusd: Some(42_000),
            pricing: None,
            long_context_pricing: None,
            pricing_provenance: None,
        }]),
    };
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 34)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let click_point = (0..34)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.shell_ui.hit(*column, *row) == Some(ShellHit::UsageManager))
        .ok_or_else(|| std::io::Error::other("usage launcher has no mouse hit region"))?;
    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );
    assert!(state.usage_ui.is_open());

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(terminal_text(&terminal).contains("Token usage & estimated cost"));
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(
        state.usage_ui.focused(),
        Some(decode::ui::usage::UsageFocus::Edit)
    );
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(
        state.usage_ui.focused(),
        Some(decode::ui::usage::UsageFocus::Close)
    );
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.usage_ui.is_open());
    Ok(())
}

#[test]
fn usage_tariff_editor_is_fully_mouse_and_keyboard_operable() -> Result<(), std::io::Error> {
    use decode::ui::usage::UsageHit;

    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.usage = UsageSnapshot {
        usage: TokenUsage {
            input_tokens: 1_000,
            cached_input_tokens: 200,
            output_tokens: 100,
            total_tokens: 1_100,
        },
        last_response_tokens: Some(1_100),
        estimated_cost_microusd: 0,
        has_unpriced_usage: true,
        pricing_configured: false,
        deployments: Arc::from([DeploymentUsageSnapshot {
            deployment: "private-azure-deployment".to_owned(),
            usage: TokenUsage {
                input_tokens: 1_000,
                cached_input_tokens: 200,
                output_tokens: 100,
                total_tokens: 1_100,
            },
            cost_microusd: None,
            pricing: None,
            long_context_pricing: None,
            pricing_provenance: None,
        }]),
    };
    state.usage_ui.open(1);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let edit = (0..38)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.usage_ui.clicked(*column, *row) == Some(UsageHit::Edit))
        .ok_or_else(|| std::io::Error::other("tariff editor has no mouse hit region"))?;
    handle_mouse(left_click(edit.0, edit.1), &mut state, &command_tx);
    assert!(state.usage_ui.is_editing());

    for character in "3.25".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    for character in "0.75".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    for character in "12.5".chars() {
        handle_key(key(KeyCode::Char(character)), &mut state, &command_tx);
    }
    for _ in 0..5 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    match command_rx.try_recv() {
        Ok(OrchestratorCommand::SetDeploymentPricing { pricing, .. }) => {
            assert_eq!(pricing.deployment(), "private-azure-deployment");
            assert_eq!(pricing.rate_snapshot().input_usd_per_million(), 3.25);
            assert_eq!(pricing.rate_snapshot().output_usd_per_million(), 12.5);
        }
        Ok(other) => panic!("unexpected command: {other:?}"),
        Err(error) => panic!("tariff command was not emitted: {error}"),
    }
    assert!(!state.usage_ui.is_editing());
    Ok(())
}

#[test]
fn usage_tariff_editor_accepts_bracketed_paste() {
    use decode::ui::usage::UsageFocus;

    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    let item = DeploymentUsageSnapshot {
        deployment: "paste-model".to_owned(),
        usage: TokenUsage::default(),
        cost_microusd: None,
        pricing: None,
        long_context_pricing: None,
        pricing_provenance: None,
    };
    state.usage = UsageSnapshot {
        deployments: Arc::from([item.clone()]),
        ..UsageSnapshot::default()
    };
    state.usage_ui.open(1);
    state.usage_ui.begin_edit(&item);
    for (focus, value) in [
        (UsageFocus::InputRate, "3.25"),
        (UsageFocus::CachedRate, "0.75"),
        (UsageFocus::OutputRate, "12.5"),
    ] {
        state.usage_ui.focus(focus);
        handle_paste(value, &mut state);
    }
    state.usage_ui.focus(UsageFocus::Save);

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetDeploymentPricing { pricing, .. })
            if pricing.deployment() == "paste-model"
                && pricing.rate_snapshot().input_usd_per_million() == 3.25
                && pricing.rate_snapshot().output_usd_per_million() == 12.5
    ));
}

#[test]
fn usage_editor_stays_bound_to_its_deployment_after_reordering() {
    use decode::ui::usage::UsageFocus;

    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    let item = |deployment: &str| DeploymentUsageSnapshot {
        deployment: deployment.to_owned(),
        usage: TokenUsage::default(),
        cost_microusd: None,
        pricing: None,
        long_context_pricing: None,
        pricing_provenance: None,
    };
    let first = item("first");
    let second = item("second");
    state.usage = UsageSnapshot {
        deployments: Arc::from([first.clone(), second.clone()]),
        ..UsageSnapshot::default()
    };
    state.usage_ui.open(2);
    state.usage_ui.begin_edit(&first);
    for (focus, value) in [
        (UsageFocus::InputRate, "1"),
        (UsageFocus::CachedRate, "1"),
        (UsageFocus::OutputRate, "1"),
    ] {
        state.usage_ui.focus(focus);
        for character in value.chars() {
            state.usage_ui.push_rate_char(character);
        }
    }
    state.usage.deployments = Arc::from([second, first]);
    state.usage_ui.focus(UsageFocus::Save);

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);

    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::SetDeploymentPricing { pricing, .. })
            if pricing.deployment() == "first"
    ));
}

#[test]
fn usage_mutations_stop_when_the_agent_becomes_busy() {
    use decode::ui::usage::{UsageFocus, UsageHit};

    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.usage = UsageSnapshot {
        deployments: Arc::from([DeploymentUsageSnapshot {
            deployment: "busy-model".to_owned(),
            usage: TokenUsage::default(),
            cost_microusd: None,
            pricing: None,
            long_context_pricing: None,
            pricing_provenance: None,
        }]),
        ..UsageSnapshot::default()
    };
    state.usage_ui.open(1);
    state.usage_ui.focus(UsageFocus::Edit);
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| state.usage_ui.clicked(column, row) == Some(UsageHit::Edit))
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.usage_ui.is_editing());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn runtime_launcher_is_visible_and_clickable_before_retrying_context_failure()
-> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.phase = AgentPhase::Error {
        message: "strict context compaction rejected the request".to_owned(),
        recoverable: true,
    };
    state.deployment = "gpt-5.6-sol".to_owned();
    state.deployment_choices = Arc::from(["gpt-5.6-sol".to_owned()]);
    state.reasoning_effort = ReasoningEffort::XHigh;
    state.context_budget = 120_000;
    state.max_context_budget = 2_000_000;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 45)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let screen = terminal_text(&terminal);
    assert!(screen.contains("Runtime"));
    assert!(screen.contains("120K"));
    let click_point = (0..45)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.shell_ui.hit(*column, *row) == Some(ShellHit::RuntimeManager))
        .ok_or_else(|| std::io::Error::other("runtime launcher has no mouse hit region"))?;

    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );

    assert!(state.runtime_ui.is_open());
    Ok(())
}

#[test]
fn runtime_apply_stops_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.deployment = "primary".to_owned();
    state.deployment_choices = Arc::from(["primary".to_owned()]);
    state.context_budget = 100_000;
    state.max_context_budget = 200_000;
    state.runtime_ui.open(
        state.deployment_choices.as_ref(),
        &state.deployment,
        state.reasoning_effort,
        false,
        state.context_budget,
        state.max_context_budget,
    );
    state.runtime_ui.advance();
    state.runtime_ui.advance();
    state.runtime_ui.advance();
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.runtime_ui.clicked(column, row) == Some(decode::ui::runtime::RuntimeHit::Primary)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn failed_turn_retry_and_abort_are_mouse_clickable_and_keep_or_reset_elapsed()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let started = Instant::now();
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(41);
    state.tick(started);
    state.tick(started + Duration::from_secs(7));
    state.phase = AgentPhase::Error {
        message: "temporary transport failure".to_owned(),
        recoverable: true,
    };
    state.tick(started + Duration::from_secs(7));
    let before_retry = state
        .eta
        .turn_elapsed(started + Duration::from_secs(30))
        .unwrap_or_default();
    assert_eq!(before_retry, Duration::from_secs(7));

    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 42)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let retry = (0..42)
        .find_map(|row| {
            (0..120)
                .find(|column| state.shell_ui.hit(*column, row) == Some(ShellHit::RetryFailedTurn))
                .map(|column| (column, row))
        })
        .ok_or_else(|| std::io::Error::other("Retry has no mouse hit region"))?;
    handle_mouse(left_click(retry.0, retry.1), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::RetryTurn { turn_id: 41 })
    ));

    state.phase = AgentPhase::Requesting;
    state.tick(started + Duration::from_secs(30));
    state.tick(started + Duration::from_secs(34));
    assert_eq!(
        state.eta.turn_elapsed(started + Duration::from_secs(34)),
        Some(Duration::from_secs(11))
    );

    state.phase = AgentPhase::Error {
        message: "failed again".to_owned(),
        recoverable: true,
    };
    state.tick(started + Duration::from_secs(34));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let abort = (0..42)
        .find_map(|row| {
            (0..120)
                .find(|column| state.shell_ui.hit(*column, row) == Some(ShellHit::AbortFailedTurn))
                .map(|column| (column, row))
        })
        .ok_or_else(|| std::io::Error::other("Abort has no mouse hit region"))?;
    handle_mouse(left_click(abort.0, abort.1), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::AbortTurn { turn_id: 41 })
    ));
    assert_eq!(
        state.eta.turn_elapsed(started + Duration::from_secs(60)),
        None
    );
    Ok(())
}

#[test]
fn fatal_turn_error_does_not_offer_or_send_retry() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.phase = AgentPhase::Error {
        message: "invalid persisted state".to_owned(),
        recoverable: false,
    };
    state.active_turn_id = Some(41);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 42)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let area = terminal.backend().buffer().area;
    assert!(!(0..area.height).any(|row| {
        (0..area.width)
            .any(|column| state.shell_ui.hit(column, row) == Some(ShellHit::RetryFailedTurn))
    }));
    assert!((0..area.height).any(|row| {
        (0..area.width)
            .any(|column| state.shell_ui.hit(column, row) == Some(ShellHit::AbortFailedTurn))
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn notification_center_is_clickable_and_every_control_has_tab_fallback()
-> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.notifications.push_unique(
        "acceptance:1".to_owned(),
        NotificationKind::NeedsAction,
        "Approval needed",
        "Review the pending action",
    );
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let click_point = (0..38)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| {
            state.shell_ui.hit(*column, *row) == Some(ShellHit::NotificationCenter)
        })
        .ok_or_else(|| std::io::Error::other("notification launcher has no mouse hit region"))?;
    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );
    assert!(state.notification_ui.is_open());

    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert_eq!(state.notifications.unread_count(), 0);
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.notifications.preferences().bell_on_action);
    for _ in 0..5 {
        handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    }
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.notification_ui.is_open());
    Ok(())
}

#[test]
fn privacy_shield_launcher_and_revision_bound_reload_have_mouse_and_tab_paths()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.conversation_epoch = 21;
    state.phase_revision = 34;
    state.privacy = PrivacySnapshot {
        revision: 3,
        policy_sha256: "0123456789abcdef".to_owned(),
        blocked_attempts: 2,
        sources: Arc::from([PrivacySourceSnapshot {
            id: "built-in",
            label: "Built-in secret patterns",
            location: "compiled".to_owned(),
            active: true,
            fail_closed: false,
            rule_count: 8,
            detail: "active".to_owned(),
        }]),
    };
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let click_point = (0..38)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.shell_ui.hit(*column, *row) == Some(ShellHit::PrivacyShield))
        .ok_or_else(|| std::io::Error::other("privacy launcher has no mouse hit region"))?;
    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );
    assert!(state.privacy_ui.is_open());
    assert_eq!(state.privacy_ui.focused(), Some(PrivacyFocus::Sources));

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.privacy_ui.focused(), Some(PrivacyFocus::Reload));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::ReloadPrivacy { scope })
            if scope.conversation_epoch == 21 && scope.phase_revision == 34
    ));

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.privacy_ui.focused(), Some(PrivacyFocus::Close));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.privacy_ui.is_open());
    Ok(())
}

#[test]
fn privacy_reload_stops_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.privacy_ui.open(0);
    state.privacy_ui.next_focus();
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;

    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.privacy_ui.clicked(column, row) == Some(decode::ui::privacy::PrivacyHit::Reload)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn rewind_confirmation_stops_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.checkpoints = Arc::from([decode::agent::CheckpointSummary {
        id: 7,
        created_at: Utc::now(),
        prompt_preview: "change".to_owned(),
        changed_paths: vec!["src/lib.rs".to_owned()],
        history_entries_before: 1,
        session_id: None,
    }]);
    state.rewind_ui.open(1);
    state.rewind_ui.review(&state.checkpoints);
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;

    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.rewind_ui.clicked(column, row) == Some(decode::ui::rewind::RewindHit::Primary)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn session_permission_launcher_and_revoke_controls_are_clickable_and_scoped()
-> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.conversation_epoch = 55;
    state.phase_revision = 89;
    state.shell_permissions = ShellPermissionSnapshot {
        revision: 1,
        grants: Arc::from([ShellCommandGrant {
            id: 13,
            command: "cargo check".to_owned(),
            command_digest: CommandDigest::for_command("cargo check"),
            granted_turn_id: 2,
            granted_action_id: 4,
        }]),
    };
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let click_point = (0..38)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.shell_ui.hit(*column, *row) == Some(ShellHit::ShellPermissions))
        .ok_or_else(|| std::io::Error::other("permission launcher has no mouse hit region"))?;
    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );
    assert!(state.permission_ui.is_open());
    assert_eq!(state.permission_ui.focused(), Some(PermissionFocus::Grants));

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.permission_ui.focused(), Some(PermissionFocus::Revoke));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::RevokeSessionShellGrant {
            grant_id: 13,
            scope,
        }) if scope.conversation_epoch == 55 && scope.phase_revision == 89
    ));

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.permission_ui.focused(), Some(PermissionFocus::Clear));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::ClearSessionShellGrants { scope })
            if scope.conversation_epoch == 55 && scope.phase_revision == 89
    ));

    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.permission_ui.focused(), Some(PermissionFocus::Close));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(!state.permission_ui.is_open());
    Ok(())
}

#[test]
fn permission_mutations_stop_when_the_agent_becomes_busy() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.shell_permissions = ShellPermissionSnapshot {
        revision: 1,
        grants: Arc::from([ShellCommandGrant {
            id: 13,
            command: "cargo check".to_owned(),
            command_digest: CommandDigest::for_command("cargo check"),
            granted_turn_id: 2,
            granted_action_id: 4,
        }]),
    };
    state.permission_ui.open(1);
    state.permission_ui.next_focus();
    state.phase = AgentPhase::Streaming;
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let area = terminal.backend().buffer().area;

    assert!(!(0..area.height).any(|row| {
        (0..area.width).any(|column| {
            state.permission_ui.clicked(column, row)
                == Some(decode::ui::permissions::PermissionHit::Revoke)
        })
    }));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[test]
fn parallel_read_activity_cards_are_animated_clickable_and_keyboard_reachable()
-> Result<(), std::io::Error> {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let mut state = AppState::new();
    state.phase = AgentPhase::ExecutingTools;
    state.tool_actions = Arc::new(BTreeMap::from([
        (
            1,
            ToolAction::ReadFile {
                path: "src/lib.rs".to_owned(),
            },
        ),
        (
            2,
            ToolAction::SearchCode {
                pattern: "PrivacyShield".to_owned(),
                path: Some("src".to_owned()),
            },
        ),
    ]));
    state.running_tools.extend([1, 2]);
    state.shell_ui.select_tab(ShellTab::Activity);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 38)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(terminal_text(&terminal).contains("2 reads in parallel"));

    let click_point = (0..38)
        .flat_map(|row| (0..120).map(move |column| (column, row)))
        .find(|(column, row)| state.shell_ui.hit(*column, *row) == Some(ShellHit::Tool(2)))
        .ok_or_else(|| std::io::Error::other("running tool card has no mouse hit region"))?;
    handle_mouse(
        left_click(click_point.0, click_point.1),
        &mut state,
        &command_tx,
    );
    assert_eq!(state.selected_tool, Some(2));
    assert!(state.expanded_tools.contains(&2));

    state.selected_tool = None;
    handle_key(key(KeyCode::Tab), &mut state, &command_tx);
    assert_eq!(state.selected_tool, Some(1));
    handle_key(key(KeyCode::Enter), &mut state, &command_tx);
    assert!(state.expanded_tools.contains(&1));
    Ok(())
}

#[test]
fn tiny_terminal_cannot_unlock_an_unseen_long_command() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = confirmation_state(&"x".repeat(1_300));
    let backend = TestBackend::new(10, 3);
    let mut terminal = infallible(Terminal::new(backend));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    handle_key(key(KeyCode::End), &mut state, &command_tx);
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn tiny_terminal_cannot_approve_even_a_short_unseen_command() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let mut state = confirmation_state("echo safe");
    let backend = TestBackend::new(10, 3);
    let mut terminal = infallible(Terminal::new(backend));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    handle_key(
        KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        &mut state,
        &command_tx,
    );
    assert!(state.pending_confirmation.is_some());
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    Ok(())
}

#[test]
fn confirmation_buttons_are_clickable_and_approval_fails_closed() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let backend = TestBackend::new(100, 30);
    let mut terminal = infallible(Terminal::new(backend));
    let mut state = confirmation_state("echo safe");
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let (approve_column, approve_row) = confirmation_choice_point(
        &state,
        ConfirmationChoice::Approve,
        Rect::new(0, 0, 100, 30),
    )
    .ok_or_else(|| std::io::Error::other("enabled approve button has no click region"))?;
    handle_mouse(
        left_click(approve_column, approve_row),
        &mut state,
        &command_tx,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::RunOnce,
            ..
        })
    ));

    let mut long_state = confirmation_state(&"x".repeat(1_300));
    infallible(terminal.draw(|frame| render::draw(frame, &mut long_state)));
    let locked_approve = confirmation_choice_point(
        &long_state,
        ConfirmationChoice::Approve,
        Rect::new(0, 0, 100, 30),
    )
    .ok_or_else(|| std::io::Error::other("long approval has no mouse review path"))?;
    handle_mouse(
        left_click(locked_approve.0, locked_approve.1),
        &mut long_state,
        &command_tx,
    );
    assert_eq!(
        long_state.confirmation_scroll,
        long_state.confirmation_max_scroll
    );
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    infallible(terminal.draw(|frame| render::draw(frame, &mut long_state)));
    let approve = confirmation_choice_point(
        &long_state,
        ConfirmationChoice::Approve,
        Rect::new(0, 0, 100, 30),
    )
    .ok_or_else(|| std::io::Error::other("reviewed approval has no mouse hit region"))?;
    handle_mouse(
        left_click(approve.0, approve.1),
        &mut long_state,
        &command_tx,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::RunOnce,
            ..
        })
    ));
    Ok(())
}

#[test]
fn exact_session_trust_is_clickable_tab_navigable_and_review_gated() -> Result<(), std::io::Error> {
    let (command_tx, mut command_rx) = mpsc::channel(3);
    let backend = TestBackend::new(110, 32);
    let mut terminal = infallible(Terminal::new(backend));
    let mut state = local_policy_confirmation_state("cargo test --all-targets");
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let (trust_column, trust_row) = confirmation_choice_point(
        &state,
        ConfirmationChoice::TrustExactForSession,
        Rect::new(0, 0, 110, 32),
    )
    .ok_or_else(|| std::io::Error::other("enabled session-trust button has no click region"))?;
    handle_mouse(left_click(trust_column, trust_row), &mut state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::TrustExactForSession,
            ..
        })
    ));

    let mut keyboard_state = local_policy_confirmation_state("cargo check");
    infallible(terminal.draw(|frame| render::draw(frame, &mut keyboard_state)));
    handle_key(key(KeyCode::Tab), &mut keyboard_state, &command_tx);
    handle_key(key(KeyCode::Enter), &mut keyboard_state, &command_tx);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::Confirm {
            decision: ShellApprovalDecision::TrustExactForSession,
            ..
        })
    ));

    let mut long_state = local_policy_confirmation_state(&"x".repeat(1_300));
    infallible(terminal.draw(|frame| render::draw(frame, &mut long_state)));
    assert!(
        confirmation_choice_point(
            &long_state,
            ConfirmationChoice::TrustExactForSession,
            Rect::new(0, 0, 110, 32),
        )
        .is_none(),
        "session trust must remain disabled until the full suffix is rendered"
    );
    Ok(())
}

fn confirmation_state(command: &str) -> AppState {
    let mut state = AppState::new();
    state.phase = AgentPhase::AwaitingConfirmation;
    state.active_turn_id = Some(11);
    state.pending_confirmation = Some(PendingConfirmation {
        turn_id: 11,
        action_id: 29,
        action: ToolAction::ExecuteCommand {
            command: command.to_owned(),
            requires_confirmation: true,
        },
        command: command.to_owned(),
        command_bytes: command.len(),
        command_digest: CommandDigest::for_command(command),
        model_requested: true,
        reason: ConfirmationReason::ModelRequested,
        session_trust_available: false,
    });
    state
}

fn local_policy_confirmation_state(command: &str) -> AppState {
    let mut state = confirmation_state(command);
    if let Some(pending) = state.pending_confirmation.as_mut() {
        pending.model_requested = false;
        pending.reason = ConfirmationReason::PolicyRequired;
        pending.session_trust_available = true;
        if let ToolAction::ExecuteCommand {
            requires_confirmation,
            ..
        } = &mut pending.action
        {
            *requires_confirmation = false;
        }
    }
    state
}

fn recovery_fleet(can_resume: bool) -> SubagentFleetSnapshot {
    let now = Utc::now();
    let agent = SubagentSnapshot {
        id: SubagentId::new(17),
        parent_id: None,
        depth: 1,
        revision: 44,
        session_id: Some("session-recovery".to_owned()),
        label: "resume writer".to_owned(),
        task: "finish isolated edit".to_owned(),
        profile_id: "builtin:writer".to_owned(),
        profile_name: "Writer".to_owned(),
        mode: SubagentMode::Writer,
        status: SubagentStatus::RecoveryRequired,
        deployment: "test-model".to_owned(),
        reasoning_effort: ReasoningEffort::High,
        created_at: now,
        started_at: Some(now),
        completed_at: None,
        updated_at: now,
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
        token_budget: 150_000,
        tool_iterations: 2,
        last_message: "restart interrupted writer".to_owned(),
        result: String::new(),
        error: None,
        worktree: can_resume.then(|| "managed/worktree-17".to_owned()),
        base_commit: can_resume.then(|| "0123456789abcdef".to_owned()),
        changed_files: Arc::from(["src/lib.rs".to_owned()]),
        resolved_files: Arc::from([]),
        change_digest: Some("digest".to_owned()),
        pending_command: None,
        pending_budget: None,
        transcript: Arc::from([]),
        recovery: Some(SubagentRecoverySummary {
            attempt: 2,
            checkpoint_at: now,
            reason: "restart interrupted writer".to_owned(),
            uncertain_action: None,
            can_resume,
        }),
        dependencies: Arc::from([]),
        file_claims: Arc::from([]),
    };
    SubagentFleetSnapshot {
        revision: 9,
        enabled: true,
        capacity: 4,
        active: 0,
        total_tokens: 15,
        token_budget: 500_000,
        availability_error: None,
        mcp_enabled: false,
        mcp_status: decode::notice::UiNotice::SubagentMcpDisabled,
        profiles: AgentProfileCatalogSnapshot::default(),
        agents: Arc::from([agent]),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn terminal_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn left_click(column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn find_ascii_in_buffer(buffer: &ratatui::buffer::Buffer, needle: &str) -> Option<(u16, u16)> {
    let needle = needle
        .chars()
        .map(|value| value.to_string())
        .collect::<Vec<_>>();
    let area = buffer.area;
    for row in area.y..area.bottom() {
        for column in area.x..area.right() {
            let remaining = usize::from(area.right().saturating_sub(column));
            if remaining < needle.len() {
                break;
            }
            if needle.iter().enumerate().all(|(offset, expected)| {
                buffer[(column.saturating_add(offset as u16), row)].symbol() == expected
            }) {
                return Some((column, row));
            }
        }
    }
    None
}

#[test]
fn safe_pause_and_resume_have_real_mouse_controls_and_keyboard_fallback_labels() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let urgent = UrgentControlHandle::default();
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(77);
    state.deployment = "test-model".to_owned();
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 42)));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(!terminal_text(&terminal).contains('⏸'));
    let pause = (0..42).find_map(|row| {
        (0..120)
            .find(|column| state.shell_ui.hit(*column, row) == Some(ShellHit::PauseTurn))
            .map(|column| (column, row))
    });
    let (column, row) = pause.expect("pause button must have a hit region");
    handle_mouse_enabled(
        left_click(column, row),
        &mut state,
        &command_tx,
        &urgent,
        true,
    );
    assert!(
        state
            .status_message
            .as_deref()
            .is_some_and(|message| message.contains("Pausing turn #77"))
    );

    state.phase = AgentPhase::Idle;
    state.active_turn_id = None;
    state.paused_turn_id = Some(77);
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    assert!(terminal_text(&terminal).contains("Abort (F8)"));
    let resume = (0..42).find_map(|row| {
        (0..120)
            .find(|column| state.shell_ui.hit(*column, row) == Some(ShellHit::ResumePausedTurn))
            .map(|column| (column, row))
    });
    let (column, row) = resume.expect("resume button must have a hit region");
    handle_mouse_enabled(
        left_click(column, row),
        &mut state,
        &command_tx,
        &urgent,
        true,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::RetryTurn { turn_id: 77 })
    ));
    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));
    let abort = (0..42).find_map(|row| {
        (0..120)
            .find(|column| state.shell_ui.hit(*column, row) == Some(ShellHit::AbortPausedTurn))
            .map(|column| (column, row))
    });
    let (column, row) = abort.expect("paused cancel button must have a hit region");
    handle_mouse_enabled(
        left_click(column, row),
        &mut state,
        &command_tx,
        &urgent,
        true,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::AbortTurn { turn_id: 77 })
    ));
    handle_key_with_control(
        KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE),
        &mut state,
        &command_tx,
        &urgent,
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::AbortTurn { turn_id: 77 })
    ));
    handle_key_with_control(
        KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
        &mut state,
        &command_tx,
        &urgent,
    );
}

#[test]
fn session_manager_opens_from_a_recoverable_error() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let mut state = AppState::new();
    state.phase = AgentPhase::Error {
        message: "network failed".to_owned(),
        recoverable: true,
    };
    state.active_turn_id = Some(7);

    handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        &mut state,
        &command_tx,
    );

    assert!(state.session_ui.is_open());
    assert!(matches!(
        command_rx.try_recv(),
        Ok(OrchestratorCommand::RefreshSessions { .. })
    ));
}

#[test]
fn wide_right_rail_does_not_cut_management_labels() {
    let mut state = AppState::new();
    let mut terminal = infallible(Terminal::new(TestBackend::new(180, 52)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    let rendered = terminal_text(&terminal);
    for label in ["Rules", "Skills", "Shield", "Permissions", "Auto approval"] {
        assert!(rendered.contains(label), "missing complete label {label:?}");
    }
}

#[test]
fn safe_pause_explanation_wraps_instead_of_being_cut_off() {
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.active_turn_id = Some(9);
    let mut terminal = infallible(Terminal::new(TestBackend::new(120, 48)));

    infallible(terminal.draw(|frame| render::draw(frame, &mut state)));

    assert!(terminal_text(&terminal).contains("later resume"));
}

fn confirmation_choice_point(
    state: &AppState,
    choice: ConfirmationChoice,
    size: Rect,
) -> Option<(u16, u16)> {
    for row in size.y..size.bottom() {
        for column in size.x..size.right() {
            if state.confirmation_ui.clicked(column, row) == Some(choice) {
                return Some((column, row));
            }
        }
    }
    None
}
