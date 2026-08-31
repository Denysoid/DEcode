use std::{path::PathBuf, sync::Arc};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use tokio::sync::mpsc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    agent::{
        FollowUpMode, FollowUpStatus, PlanDecision, ReviewFindingDecision, ShellApprovalDecision,
        SubagentFileDecision,
        orchestrator::{CommandScope, OrchestratorCommand, UrgentControlHandle},
    },
    attachments::{AttachmentDraft, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS_PER_TURN},
    clipboard,
    lsp::LspConnectionState,
    mcp::McpConnectionState,
    usage::{CostCoverage, DeploymentUsageSnapshot},
};

const LARGE_PASTE_BYTES: usize = 128 * 1024;

use super::{
    agents::{AgentBrowseFocus, AgentDecisionFocus, AgentDialogFocus, AgentEditor, AgentHit},
    app::AppState,
    approval_center::{ApprovalFocus, ApprovalHit},
    automation::{
        AutomationFocus, AutomationHit, AutomationPane, item_count as automation_item_count,
    },
    code_index::{CodeIndexFocus, CodeIndexHitRegion},
    confirm::{ConfirmationChoice, ContinuationChoice},
    connections::ConnectionField,
    followups::{FollowUpFocus, FollowUpHit, FollowUpStage},
    github::{GitHubFocus, GitHubHit, GitHubStage},
    i18n::{self, Text},
    instructions::{InstructionsFocus, InstructionsHit},
    language::{LanguageFocus, LanguageHit},
    lsp::{LspFocus, LspHit, LspPane},
    mcp::{McpFocus, McpHit},
    modes::{GoalEditorFocus, ModesFocus, ModesHit, PlanFocus, PlanHit},
    notifications::{NotificationFocus, NotificationHit},
    palette::{
        FilePaletteAction, PaletteCommandSelection, PaletteFocus, PaletteHit, PaletteMode,
        command_matches,
    },
    patch_review::{PatchReviewFocus, PatchReviewHit},
    permissions::{PermissionFocus, PermissionHit},
    plugins::{PluginFocus, PluginHit, marketplace_entry, marketplace_source},
    privacy::{PrivacyFocus, PrivacyHit},
    review::{ReviewFocus, ReviewHit},
    rewind::{RewindFocus, RewindHit, RewindStage},
    runtime::{RuntimeFocus, RuntimeHit, RuntimeStage},
    sessions::{SessionFocus, SessionHit, SessionIntent, SessionStage},
    shell::{ShellHit, ShellTab},
    side_chat::{SideFocus, SideHit, SideStage},
    skills::{SkillsFocus, SkillsHit},
    terminal::{TerminalHit, TerminalInputMode, terminal_control_error_text},
    usage::{UsageFocus, UsageHit},
};

pub fn handle_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    handle_key_inner(key, state, command_tx, None);
}

pub fn handle_key_with_control(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: &UrgentControlHandle,
) {
    handle_key_inner(key, state, command_tx, Some(urgent_control));
}

fn handle_key_inner(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if matches!(key.kind, KeyEventKind::Release) {
        return;
    }
    state.mascot.interact(std::time::Instant::now());

    // Urgent controls must not wait behind a modal or a full command queue.
    if state.has_blocking_modal() {
        if is_control_char(key, 'c') && state.phase.is_busy() {
            interrupt_or_quit(state, command_tx, urgent_control);
            return;
        }
        if is_control_char(key, 'r') {
            reset_from_modal(state, command_tx, urgent_control);
            return;
        }
    }

    if state.pending_plan_review.is_some() {
        handle_plan_review_key(key, state, command_tx);
        return;
    }
    if state.pending_confirmation_ids().is_some() {
        handle_confirmation_key(key, state, command_tx, urgent_control);
        return;
    }
    if state.pending_subagent_review.is_some() {
        handle_subagent_review_key(key, state, command_tx);
        return;
    }
    if state.pending_patch_review.is_some() {
        handle_patch_review_key(key, state, command_tx, urgent_control);
        return;
    }
    if state
        .subagents
        .agents
        .iter()
        .any(|agent| agent.pending_command.is_some())
    {
        handle_subagent_command_key(key, state, command_tx);
        return;
    }
    if !matches!(state.agents_ui.editor(), AgentEditor::Closed) {
        handle_agent_editor_key(key, state, command_tx);
        return;
    }
    if state.pending_continuation.is_some() {
        handle_continuation_key(key, state, command_tx, urgent_control);
        return;
    }
    if state.runtime_ui.is_open() {
        handle_runtime_key(key, state, command_tx);
        return;
    }
    if state.language_ui.is_open() {
        handle_language_key(key, state);
        return;
    }
    if state.mcp_ui.is_open() {
        handle_mcp_key(key, state, command_tx);
        return;
    }
    if state.lsp_ui.is_open() {
        handle_lsp_key(key, state, command_tx);
        return;
    }
    if state.code_index_ui.is_open() {
        handle_code_index_key(key, state, command_tx);
        return;
    }
    if state.privacy_ui.is_open() {
        handle_privacy_key(key, state, command_tx);
        return;
    }
    if state.permission_ui.is_open() {
        handle_permission_key(key, state, command_tx);
        return;
    }
    if state.side_chat_ui.is_open() {
        handle_side_chat_key(key, state, command_tx);
        return;
    }
    if state.follow_up_ui.is_open() {
        handle_follow_up_key(key, state, command_tx);
        return;
    }
    if state.review_ui.is_open() {
        handle_review_key(key, state, command_tx);
        return;
    }
    if state.notification_ui.is_open() {
        handle_notification_key(key, state);
        return;
    }
    if state.github_ui.is_open() {
        handle_github_key(key, state, command_tx);
        return;
    }
    if state.usage_ui.is_open() {
        handle_usage_key(key, state, command_tx);
        return;
    }
    if state.modes_ui.is_open() {
        handle_modes_key(key, state, command_tx);
        return;
    }
    if state.instructions_ui.is_open() {
        handle_instructions_key(key, state, command_tx);
        return;
    }
    if state.skills_ui.is_open() {
        handle_skills_key(key, state, command_tx);
        return;
    }
    if state.automation_ui.is_open() {
        handle_automation_key(key, state, command_tx);
        return;
    }
    if state.plugin_ui.is_open() {
        handle_plugin_key(key, state, command_tx);
        return;
    }
    if state.approval_center_ui.is_open() {
        handle_approval_center_key(key, state, command_tx);
        return;
    }
    if state.palette_ui.is_open() {
        handle_palette_key(key, state, command_tx, urgent_control);
        return;
    }
    if state.session_ui.is_open() {
        handle_session_key(key, state, command_tx);
        return;
    }
    if state.rewind_ui.is_open() {
        handle_rewind_key(key, state, command_tx);
        return;
    }
    if state.shell_ui.tool_menu_is_open() {
        if let Some((action_id, action)) = state.shell_ui.handle_tool_menu_key(&key) {
            handle_tool_menu_action(action_id, &action, state);
        }
        return;
    }
    if state.shell_ui.menu_is_open() {
        if let Some(action) = state.shell_ui.handle_menu_key(
            &key,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
            state.mascot.enabled(),
            state.show_thinking,
            state.show_tool_activity,
        ) {
            handle_menu_action(&action, state, command_tx, urgent_control);
        }
        return;
    }

    if matches!(key.code, KeyCode::F(10)) {
        state.shell_ui.open_menu();
        return;
    }
    if matches!(key.code, KeyCode::F(6)) {
        pause_or_resume(state, command_tx, urgent_control);
        return;
    }
    if matches!(key.code, KeyCode::F(8)) && state.paused_turn_id.is_some() {
        abort_paused_turn(state, command_tx);
        return;
    }
    if matches!(key.code, KeyCode::F(7)) {
        if !state.mascot.enabled() || state.phase.is_busy() {
            return;
        }
        let now = std::time::Instant::now();
        if matches!(
            state.mascot.mood(&state.phase, now),
            super::mascot::MascotMood::Sleeping
        ) {
            state.mascot.wake(now);
            state.status_message = Some(i18n::text(Text::Wake).to_owned());
        } else {
            let reaction = state.mascot.feed(now);
            state.status_message = Some(i18n::text(reaction.status_key()).to_owned());
        }
        return;
    }
    if matches!(key.code, KeyCode::Tab) && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.shell_ui.next_tab();
        return;
    }
    if matches!(key.code, KeyCode::BackTab) && key.modifiers.contains(KeyModifiers::CONTROL) {
        state.shell_ui.previous_tab();
        return;
    }
    if terminal_shortcut(key, 't') {
        state.shell_ui.select_tab(ShellTab::Terminal);
        handle_terminal_key(key, state);
        return;
    }
    if state.shell_ui.active_tab() == ShellTab::Terminal {
        handle_terminal_key(key, state);
        return;
    }

    if state.phase.is_error() && state.input_buffer.is_empty() {
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                state.shell_ui.toggle_failed_action_focus();
                return;
            }
            KeyCode::Enter => {
                send_failed_turn_decision(state, command_tx, state.shell_ui.retry_is_focused());
                return;
            }
            _ => {}
        }
    }

    if is_control_char(key, 'c') {
        interrupt_or_quit(state, command_tx, urgent_control);
        return;
    }
    if is_control_char(key, 'r') {
        if let Some(control) = urgent_control {
            control.reset();
            state.status_message = Some(i18n::text(Text::ResetRequested).to_owned());
        } else if try_send(command_tx, OrchestratorCommand::Reset, state) {
            state.status_message = Some(i18n::text(Text::ResetRequested).to_owned());
        }
        return;
    }
    if is_control_char(key, 'z') {
        open_rewind(state);
        return;
    }
    if is_control_char(key, 'o') {
        open_sessions(state, command_tx);
        return;
    }
    if is_control_char(key, 'n') {
        start_new_session(state, command_tx);
        return;
    }
    if is_control_char(key, 'v') && try_attach_clipboard_image(state) {
        return;
    }
    if is_control_char(key, 'm') {
        open_runtime(state);
        return;
    }
    if is_control_char(key, 'k') {
        open_mcp(state);
        return;
    }
    if is_control_char(key, 'l') {
        open_lsp(state, command_tx);
        return;
    }
    if is_control_char(key, 'b') {
        open_code_index(state, command_tx);
        return;
    }
    if is_control_char(key, 'g') {
        open_modes(state);
        return;
    }
    if matches!(key.code, KeyCode::Char('/')) && state.input_buffer.is_empty() {
        open_palette(PaletteMode::Commands, state);
        return;
    }
    if matches!(key.code, KeyCode::Char('@')) {
        open_palette(PaletteMode::Files, state);
        return;
    }
    if is_control_char(key, 'y') {
        open_side_chat(state);
        return;
    }
    if is_control_char(key, 'j') {
        open_follow_ups(state, false);
        return;
    }
    if state.shell_ui.active_tab() == ShellTab::Agents {
        handle_agents_key(key, state, command_tx);
        return;
    }

    match key.code {
        KeyCode::Esc => interrupt_or_quit(state, command_tx, urgent_control),
        KeyCode::Char('r' | 'R')
            if state.phase.is_error() && key.modifiers.contains(KeyModifiers::ALT) =>
        {
            send_failed_turn_decision(state, command_tx, true);
        }
        KeyCode::Char('a' | 'A')
            if state.phase.is_error() && key.modifiers.contains(KeyModifiers::ALT) =>
        {
            send_failed_turn_decision(state, command_tx, false);
        }
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => insert_text(state, "\n"),
        KeyCode::Char('j' | 'J') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            insert_text(state, "\n");
        }
        KeyCode::Enter => {
            if state.input_buffer.is_empty() && toggle_selected_tool(state) {
                return;
            }
            submit_prompt(state, command_tx);
        }
        KeyCode::Backspace => backspace_grapheme(state),
        KeyCode::Delete => delete_grapheme(state),
        KeyCode::Left => move_left(state),
        KeyCode::Right => move_right(state),
        KeyCode::Home => move_line_start(state),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => jump_to_latest(state),
        KeyCode::End => move_line_end(state),
        KeyCode::Up if state.input_buffer.contains('\n') => move_vertical(state, false),
        KeyCode::Down if state.input_buffer.contains('\n') => move_vertical(state, true),
        KeyCode::Tab if state.input_buffer.is_empty() => select_next_tool(state),
        KeyCode::BackTab if state.input_buffer.is_empty() => select_previous_tool(state),
        KeyCode::Char(character)
            if state.can_whip()
                && state.input_buffer.is_empty()
                && character.eq_ignore_ascii_case(&state.whip_hotkey) =>
        {
            send_whip(state, command_tx, urgent_control);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            let mut encoded = [0_u8; 4];
            insert_text(state, character.encode_utf8(&mut encoded));
            let _ = attach_completed_composer_path(state);
        }
        KeyCode::Up => scroll_history(state, -1),
        KeyCode::Down => scroll_history(state, 1),
        KeyCode::PageUp => scroll_history(state, -10),
        KeyCode::PageDown => scroll_history(state, 10),
        _ => {}
    }
}

fn try_attach_clipboard_image(state: &mut AppState) -> bool {
    if state.pending_attachments.len() >= MAX_ATTACHMENTS_PER_TURN {
        state.status_message = Some(format!(
            "{}: {MAX_ATTACHMENTS_PER_TURN}",
            i18n::text(Text::AttachmentHint)
        ));
        return true;
    }
    let image = match clipboard::read_image_png() {
        Ok(Some(image)) => image,
        Ok(None) => return false,
        Err(error) => {
            state.status_message = Some(format!("{}: {error}", i18n::text(Text::Failed)));
            return true;
        }
    };
    let filename = format!(
        "clipboard-{}x{}-{}.png",
        image.width,
        image.height,
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
    );
    let Some(draft) = AttachmentDraft::from_clipboard_png(image.png_bytes, filename) else {
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::AttachmentHint),
            i18n::text(Text::Failed)
        ));
        return true;
    };
    attach_draft(state, draft);
    true
}

/// Bracketed paste is a single editor operation. Newlines are deliberately
/// retained; carriage-return line endings are normalized for predictable editing.
pub fn handle_paste(text: &str, state: &mut AppState) {
    handle_paste_inner(text, state, None);
}

pub fn handle_paste_with_commands(
    text: &str,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    handle_paste_inner(text, state, Some(command_tx));
}

fn handle_paste_inner(
    text: &str,
    state: &mut AppState,
    command_tx: Option<&mpsc::Sender<OrchestratorCommand>>,
) {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if state.mcp_ui.is_editing() {
        state.mcp_ui.editor_mut().push_text(&normalized);
        return;
    }
    if state.lsp_ui.is_editing() {
        state.lsp_ui.editor_mut().push_text(&normalized);
        return;
    }
    if state.pending_plan_review.is_some() && state.plan_approval_ui.is_editing() {
        state.plan_approval_ui.push_text(&normalized);
        return;
    }
    if state.modes_ui.is_open() && state.modes_ui.is_editing_goal() {
        state.modes_ui.push_goal_text(&normalized);
        return;
    }
    if state.code_index_ui.is_open() && state.code_index_ui.focused() == Some(CodeIndexFocus::Query)
    {
        state
            .code_index_ui
            .push_text(&normalized.replace('\n', " "));
        return;
    }
    if state.palette_ui.is_open() {
        state
            .palette_ui
            .push_query_text(&normalized.replace('\n', " "));
        update_palette_total(state);
        return;
    }
    if state.side_chat_ui.is_open()
        && state.side_chat_ui.stage() == SideStage::Compose
        && state.side_chat_ui.focused() == Some(SideFocus::Question)
    {
        state.side_chat_ui.push_text(&normalized);
        return;
    }
    if state.follow_up_ui.is_open()
        && matches!(
            state.follow_up_ui.stage(),
            FollowUpStage::Compose | FollowUpStage::Edit
        )
        && state.follow_up_ui.focused() == Some(FollowUpFocus::Editor)
    {
        state.follow_up_ui.push_text(&normalized);
        return;
    }
    if state.plugin_ui.is_open() && state.plugin_ui.focused() == Some(PluginFocus::Input) {
        state
            .plugin_ui
            .push_input_text(&normalized.replace('\n', " "));
        return;
    }
    if state.session_ui.is_open() {
        let single_line = normalized.replace('\n', " ");
        match state.session_ui.stage() {
            SessionStage::Picker => {
                state.session_ui.push_query_text(&single_line);
                if let Some(command_tx) = command_tx {
                    refresh_sessions(state, command_tx);
                }
            }
            SessionStage::Rename => state.session_ui.push_rename_text(&single_line),
            SessionStage::Closed | SessionStage::Actions | SessionStage::WorkspaceConfirm => {}
        }
        return;
    }
    if state.usage_ui.is_open() && state.usage_ui.is_editing() {
        state.usage_ui.push_rate_text(&normalized);
        return;
    }
    if !matches!(state.agents_ui.editor(), AgentEditor::Closed) {
        for character in normalized.chars() {
            state.agents_ui.push(character);
        }
        return;
    }
    if state.has_blocking_modal() {
        return;
    }
    if state.shell_ui.active_tab() == ShellTab::Terminal {
        let fleet = state.terminal.clone();
        match state.terminal_ui.paste(text, &fleet) {
            Ok(true) => {}
            Ok(false) => {
                state.status_message = Some(i18n::text(Text::NoTerminalOpen).to_owned());
            }
            Err(error) => {
                state.status_message = Some(terminal_control_error_text(&error));
            }
        }
        return;
    }
    if try_attach_pasted_files(&normalized, state) || try_attach_large_paste(text, state) {
        return;
    }
    insert_text(state, &normalized);
}

fn try_attach_pasted_files(text: &str, state: &mut AppState) -> bool {
    let Some(paths) = pasted_paths(text) else {
        return false;
    };
    if state.pending_attachments.len().saturating_add(paths.len()) > MAX_ATTACHMENTS_PER_TURN {
        state.status_message = Some(format!(
            "{}: {MAX_ATTACHMENTS_PER_TURN}",
            i18n::text(Text::AttachmentHint)
        ));
        return true;
    }
    let drafts = paths
        .into_iter()
        .map(AttachmentDraft::snapshot_user_selected_path)
        .collect::<Result<Vec<_>, _>>();
    let drafts = match drafts {
        Ok(drafts) => drafts,
        Err(error) => {
            state.status_message = Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
            return true;
        }
    };
    for draft in drafts {
        attach_draft(state, draft);
    }
    true
}

fn pasted_paths(text: &str) -> Option<Vec<PathBuf>> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 32 * 1024 {
        return None;
    }
    if let Some(path) = pasted_path(trimmed)
        && is_attachable_path(&path)
    {
        return Some(vec![path]);
    }

    let tokens = split_pasted_path_tokens(trimmed)?;
    if tokens.len() < 2 {
        return None;
    }
    tokens
        .into_iter()
        .map(|token| pasted_path(&token))
        .collect::<Option<Vec<_>>>()
        .filter(|paths| paths.iter().all(|path| is_attachable_path(path)))
}

fn pasted_path(value: &str) -> Option<PathBuf> {
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value);
    let path = if value
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
    {
        url::Url::parse(value).ok()?.to_file_path().ok()?
    } else {
        PathBuf::from(value)
    };
    path.is_absolute().then_some(path)
}

fn is_attachable_path(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

fn split_pasted_path_tokens(text: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in text.chars() {
        match quote {
            Some(expected) if character == expected => quote = None,
            Some(_) => current.push(character),
            None if current.is_empty() && matches!(character, '"' | '\'') => {
                quote = Some(character);
            }
            None if character.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(character),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

fn try_attach_large_paste(text: &str, state: &mut AppState) -> bool {
    if text.len() < LARGE_PASTE_BYTES {
        return false;
    }
    if text.len() > MAX_ATTACHMENT_BYTES {
        state.status_message = Some(format!(
            "{}: text exceeds the 50 MiB attachment limit",
            i18n::text(Text::Failed)
        ));
        return true;
    }
    let filename = format!(
        "pasted-text-{}.txt",
        chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f")
    );
    let Some(draft) = AttachmentDraft::from_pasted_bytes(text.as_bytes().to_vec(), filename) else {
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::AttachmentHint),
            i18n::text(Text::Failed)
        ));
        return true;
    };
    attach_draft(state, draft);
    true
}

fn trailing_pasted_path(text: &str) -> Option<(usize, PathBuf)> {
    let candidate_end = text.trim_end().len();
    let candidate_text = &text[..candidate_end];
    candidate_text
        .char_indices()
        .rev()
        .filter(|(start, _)| {
            let suffix = &candidate_text[*start..];
            let previous = candidate_text[..*start].chars().next_back();
            *start == 0
                || previous.is_some_and(char::is_whitespace)
                || (!matches!(previous, Some('"' | '\'')) && looks_like_absolute_path_start(suffix))
        })
        .find_map(|(start, _)| {
            let path = pasted_path(&candidate_text[start..])?;
            is_attachable_path(&path).then_some((start, path))
        })
}

fn looks_like_absolute_path_start(value: &str) -> bool {
    let value = value
        .strip_prefix('"')
        .or_else(|| value.strip_prefix('\''))
        .unwrap_or(value);
    let bytes = value.as_bytes();
    let drive_root = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    drive_root
        || value.starts_with('/')
        || value.starts_with('\\')
        || value
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file://"))
}

fn attach_completed_composer_path(state: &mut AppState) -> Option<bool> {
    if state.input_buffer.len() > 32 * 1024 {
        return None;
    }
    let (prefix_len, path) = trailing_pasted_path(&state.input_buffer)?;
    if state.pending_attachments.len() >= MAX_ATTACHMENTS_PER_TURN {
        state.status_message = Some(format!(
            "{}: {MAX_ATTACHMENTS_PER_TURN}",
            i18n::text(Text::AttachmentHint)
        ));
        return Some(false);
    }
    let draft = match AttachmentDraft::snapshot_user_selected_path(path) {
        Ok(draft) => draft,
        Err(error) => {
            state.status_message = Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
            return Some(false);
        }
    };
    let duplicate = state
        .pending_attachments
        .iter()
        .any(|candidate| candidate.source == draft.source);
    let previous_count = state.pending_attachments.len();
    attach_draft(state, draft);
    if state.pending_attachments.len() == previous_count && !duplicate {
        return Some(false);
    }
    state.input_buffer.truncate(prefix_len);
    state.input_cursor = state.input_buffer.len();
    Some(true)
}

fn prepare_composer_attachments(state: &mut AppState) -> bool {
    if let Some(attached) = attach_completed_composer_path(state)
        && !attached
    {
        return false;
    }
    let candidate = state.input_buffer.clone();
    let previous_count = state.pending_attachments.len();
    let handled =
        try_attach_pasted_files(&candidate, state) || try_attach_large_paste(&candidate, state);
    if !handled {
        return true;
    }
    if state.pending_attachments.len() == previous_count {
        return false;
    }
    state.input_buffer.clear();
    state.input_cursor = 0;
    true
}

fn handle_confirmation_mouse(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(choice) = state.confirmation_ui.clicked(mouse.column, mouse.row) else {
                return;
            };
            state.confirmation_ui.focus(choice);
            decide_confirmation(choice, state, command_tx);
        }
        MouseEventKind::ScrollUp
            if state
                .confirmation_ui
                .command_contains(mouse.column, mouse.row) =>
        {
            state.confirmation_end_requested = false;
            state.confirmation_scroll = state.confirmation_scroll.saturating_sub(3);
        }
        MouseEventKind::ScrollDown
            if state
                .confirmation_ui
                .command_contains(mouse.column, mouse.row) =>
        {
            scroll_confirmation(state, 3);
        }
        _ => {}
    }
}

pub fn handle_mouse(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    handle_mouse_inner(mouse, state, command_tx, None);
}

pub fn handle_mouse_with_control(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: &UrgentControlHandle,
) {
    handle_mouse_if_enabled(mouse, state, command_tx, Some(urgent_control), true);
}

pub fn handle_mouse_enabled(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: &UrgentControlHandle,
    enabled: bool,
) {
    handle_mouse_if_enabled(mouse, state, command_tx, Some(urgent_control), enabled);
}

fn handle_mouse_if_enabled(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    handle_mouse_inner(mouse, state, command_tx, urgent_control);
}

fn handle_mouse_inner(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    state.mascot.interact(std::time::Instant::now());
    if state.pending_plan_review.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.plan_approval_ui.clicked(mouse.column, mouse.row)
        {
            handle_plan_review_hit(hit, state, command_tx);
        }
        return;
    }
    if state.pending_confirmation_ids().is_some() {
        handle_confirmation_mouse(mouse, state, command_tx);
        return;
    }
    if state.pending_subagent_review.is_some() {
        handle_subagent_review_mouse(mouse, state, command_tx);
        return;
    }
    if state.pending_patch_review.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.patch_review_ui.clicked(mouse.column, mouse.row)
        {
            handle_patch_review_hit(hit, state, command_tx);
        }
        return;
    }
    if state
        .subagents
        .agents
        .iter()
        .any(|agent| agent.pending_command.is_some())
    {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.agents_ui.clicked(mouse.column, mouse.row)
        {
            handle_subagent_command_hit(hit, state, command_tx);
        }
        return;
    }
    if !matches!(state.agents_ui.editor(), AgentEditor::Closed) {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.agents_ui.clicked(mouse.column, mouse.row)
        {
            handle_agent_hit(hit, state, command_tx);
        }
        return;
    }
    if state.pending_continuation.is_some() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(choice) = state.continuation_ui.clicked(mouse.column, mouse.row)
        {
            state.continuation_ui.focus(choice);
            decide_continuation(
                matches!(choice, ContinuationChoice::Continue),
                state,
                command_tx,
            );
        }
        return;
    }
    if state.runtime_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.runtime_ui.clicked(mouse.column, mouse.row)
        {
            handle_runtime_hit(hit, state, command_tx);
        }
        return;
    }
    if state.language_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.language_ui.clicked(mouse.column, mouse.row)
        {
            handle_language_hit(hit, state);
        }
        return;
    }
    if state.mcp_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if state.mcp_ui.is_editing() {
                if let Some(field) = state.mcp_ui.editor().clicked(mouse.column, mouse.row) {
                    handle_mcp_editor_field(field, state, command_tx);
                }
            } else if let Some(hit) = state.mcp_ui.clicked(mouse.column, mouse.row) {
                handle_mcp_hit(hit, state, command_tx);
            }
        }
        return;
    }
    if state.lsp_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            if state.lsp_ui.is_editing() {
                if let Some(field) = state.lsp_ui.editor().clicked(mouse.column, mouse.row) {
                    handle_lsp_editor_field(field, state, command_tx);
                }
            } else if let Some(hit) = state.lsp_ui.clicked(mouse.column, mouse.row) {
                handle_lsp_hit(hit, state, command_tx);
            }
        }
        return;
    }
    if state.code_index_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.code_index_ui.clicked(mouse.column, mouse.row) {
                    handle_code_index_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state.code_index_ui.previous_result(),
            MouseEventKind::ScrollDown => state.code_index_ui.next_result(),
            _ => {}
        }
        return;
    }
    if state.privacy_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.privacy_ui.clicked(mouse.column, mouse.row)
        {
            handle_privacy_hit(hit, state, command_tx);
        }
        return;
    }
    if state.permission_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.permission_ui.clicked(mouse.column, mouse.row)
        {
            handle_permission_hit(hit, state, command_tx);
        }
        return;
    }
    if state.side_chat_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.side_chat_ui.clicked(mouse.column, mouse.row) {
                    handle_side_chat_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => {
                if state.side_chat_ui.focused() == Some(SideFocus::History) {
                    state.side_chat_ui.previous_item();
                } else {
                    state.side_chat_ui.scroll_answer(-3);
                }
            }
            MouseEventKind::ScrollDown => {
                if state.side_chat_ui.focused() == Some(SideFocus::History) {
                    state.side_chat_ui.next_item();
                } else {
                    state.side_chat_ui.scroll_answer(3);
                }
            }
            _ => {}
        }
        return;
    }
    if state.follow_up_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.follow_up_ui.clicked(mouse.column, mouse.row) {
                    handle_follow_up_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => {
                if state.follow_up_ui.focused() == Some(FollowUpFocus::Items) {
                    state.follow_up_ui.previous_item();
                } else {
                    state.follow_up_ui.scroll_detail(-3);
                }
            }
            MouseEventKind::ScrollDown => {
                if state.follow_up_ui.focused() == Some(FollowUpFocus::Items) {
                    state.follow_up_ui.next_item();
                } else {
                    state.follow_up_ui.scroll_detail(3);
                }
            }
            _ => {}
        }
        return;
    }
    if state.review_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.review_ui.clicked(mouse.column, mouse.row) {
                    handle_review_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => {
                if state.review_ui.focused() == Some(ReviewFocus::Findings) {
                    state.review_ui.select_previous_finding();
                } else {
                    state.review_ui.scroll_detail(-3);
                }
            }
            MouseEventKind::ScrollDown => {
                if state.review_ui.focused() == Some(ReviewFocus::Findings) {
                    state.review_ui.select_next_finding(&state.reviews);
                } else {
                    state.review_ui.scroll_detail(3);
                }
            }
            _ => {}
        }
        return;
    }
    if state.notification_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.notification_ui.clicked(mouse.column, mouse.row) {
                    handle_notification_hit(hit, state);
                }
            }
            MouseEventKind::ScrollUp => state.notification_ui.previous_item(),
            MouseEventKind::ScrollDown => state.notification_ui.next_item(),
            _ => {}
        }
        return;
    }
    if state.github_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.github_ui.clicked(mouse.column, mouse.row) {
                    handle_github_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state.github_ui.previous_item(),
            MouseEventKind::ScrollDown => state.github_ui.next_item(),
            _ => {}
        }
        return;
    }
    if state.usage_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.usage_ui.clicked(mouse.column, mouse.row) {
                    handle_usage_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state.usage_ui.previous_item(),
            MouseEventKind::ScrollDown => state.usage_ui.next_item(),
            _ => {}
        }
        return;
    }
    if state.modes_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.modes_ui.clicked(mouse.column, mouse.row)
        {
            handle_modes_hit(hit, state, command_tx);
        }
        return;
    }
    if state.instructions_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.instructions_ui.clicked(mouse.column, mouse.row) {
                    handle_instructions_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state
                .instructions_ui
                .previous_source(state.instructions.sources.len()),
            MouseEventKind::ScrollDown => state
                .instructions_ui
                .next_source(state.instructions.sources.len()),
            _ => {}
        }
        return;
    }
    if state.skills_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.skills_ui.clicked(mouse.column, mouse.row) {
                    handle_skills_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => {
                state.skills_ui.previous_skill(state.skills.skills.len());
            }
            MouseEventKind::ScrollDown => {
                state.skills_ui.next_skill(state.skills.skills.len());
            }
            _ => {}
        }
        return;
    }
    if state.automation_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.automation_ui.clicked(mouse.column, mouse.row) {
                    handle_automation_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => {
                let total = automation_item_count(&state.automation, state.automation_ui.pane());
                state.automation_ui.previous_item(total);
            }
            MouseEventKind::ScrollDown => {
                let total = automation_item_count(&state.automation, state.automation_ui.pane());
                state.automation_ui.next_item(total);
            }
            _ => {}
        }
        return;
    }
    if state.plugin_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.plugin_ui.clicked(mouse.column, mouse.row) {
                    handle_plugin_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state.plugin_ui.move_selection(&state.plugins, false),
            MouseEventKind::ScrollDown => state.plugin_ui.move_selection(&state.plugins, true),
            _ => {}
        }
        return;
    }
    if state.approval_center_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.approval_center_ui.clicked(mouse.column, mouse.row)
        {
            handle_approval_center_hit(hit, state, command_tx);
        }
        return;
    }
    if state.palette_ui.is_open() {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.palette_ui.clicked(mouse.column, mouse.row) {
                    handle_palette_hit(hit, state, command_tx, urgent_control);
                }
            }
            MouseEventKind::ScrollUp => state.palette_ui.previous_item(),
            MouseEventKind::ScrollDown => state.palette_ui.next_item(),
            _ => {}
        }
        return;
    }
    if state.session_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.session_ui.clicked(mouse.column, mouse.row)
        {
            handle_session_hit(hit, state, command_tx);
        }
        return;
    }
    if state.rewind_ui.is_open() {
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
            && let Some(hit) = state.rewind_ui.clicked(mouse.column, mouse.row)
        {
            handle_rewind_hit(hit, state, command_tx);
        }
        return;
    }

    if state.shell_ui.tool_menu_is_open() {
        if let Some((action_id, action)) = state.shell_ui.handle_tool_menu_mouse(&mouse) {
            handle_tool_menu_action(action_id, &action, state);
        }
        return;
    }

    let menu_was_open = state.shell_ui.menu_is_open();
    if let Some(action) = state.shell_ui.handle_menu_mouse(
        &mouse,
        matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        state.mascot.enabled(),
        state.show_thinking,
        state.show_tool_activity,
    ) {
        handle_menu_action(&action, state, command_tx, urgent_control);
        return;
    }
    if menu_was_open || mouse.row == 0 {
        return;
    }
    if state.shell_ui.handle_tab_mouse(&mouse) {
        return;
    }

    if state.shell_ui.active_tab() == ShellTab::Terminal {
        handle_terminal_mouse(mouse, state);
        return;
    }

    if state.shell_ui.active_tab() == ShellTab::Agents {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = state.agents_ui.clicked(mouse.column, mouse.row) {
                    handle_agent_hit(hit, state, command_tx);
                }
            }
            MouseEventKind::ScrollUp => state.agents_ui.previous_item(&state.subagents),
            MouseEventKind::ScrollDown => state.agents_ui.next_item(&state.subagents),
            _ => {}
        }
        return;
    }

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Right) => {
            if let Some(ShellHit::Tool(action_id)) = state.shell_ui.hit(mouse.column, mouse.row) {
                state
                    .shell_ui
                    .open_tool_menu(action_id, mouse.column, mouse.row);
                state.selected_tool = Some(action_id);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(hit) = state.shell_ui.hit(mouse.column, mouse.row) {
                handle_shell_hit(hit, state, command_tx, urgent_control);
                return;
            }
            if let Some(hit) = state.session_ui.clicked(mouse.column, mouse.row) {
                handle_session_hit(hit, state, command_tx);
                return;
            }
            if let Some(hit) = state.rewind_ui.clicked(mouse.column, mouse.row) {
                handle_rewind_hit(hit, state, command_tx);
                return;
            }
            let inside_hitbox = state
                .whip_hitbox
                .is_some_and(|rect| rect_contains(rect, mouse.column, mouse.row));
            if inside_hitbox && state.can_whip() {
                send_whip(state, command_tx, urgent_control);
            }
        }
        MouseEventKind::ScrollUp => scroll_history(state, -3),
        MouseEventKind::ScrollDown => scroll_history(state, 3),
        _ => {}
    }
}

fn handle_terminal_key(key: KeyEvent, state: &mut AppState) {
    if matches!(key.code, KeyCode::F(6)) {
        state.terminal_ui.toggle_input_mode();
        state.status_message = Some(match state.terminal_ui.input_mode() {
            TerminalInputMode::Input => i18n::text(Text::InputMode).to_owned(),
            TerminalInputMode::Toolbar => i18n::text(Text::TerminalControlsHelp).to_owned(),
        });
        return;
    }
    if terminal_shortcut(key, 't') {
        let fleet = state.terminal.clone();
        let result = state.terminal_ui.create(&fleet);
        report_terminal_action(result, state);
        return;
    }
    if terminal_shortcut(key, 'x') {
        let result = state.terminal_ui.stop();
        report_terminal_action(result, state);
        return;
    }
    if terminal_shortcut(key, 'w') {
        let result = state.terminal_ui.close();
        report_terminal_action(result, state);
        return;
    }

    if state.terminal_ui.input_mode() == TerminalInputMode::Toolbar {
        match key.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Down => state.terminal_ui.next_control(),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Up => {
                state.terminal_ui.previous_control();
            }
            KeyCode::Esc => state.terminal_ui.focus_input(),
            KeyCode::Enter => {
                if let Some(focus) = state.terminal_ui.selected_control() {
                    let fleet = state.terminal.clone();
                    let result = state.terminal_ui.activate_control(focus, &fleet);
                    report_terminal_action(result, state);
                }
            }
            _ => {}
        }
        return;
    }

    let fleet = state.terminal.clone();
    match state.terminal_ui.send_key(key, &fleet) {
        Ok(true) => {}
        Ok(false) => {
            if state.terminal.sessions.is_empty() {
                state.status_message = Some(i18n::text(Text::NoTerminalOpen).to_owned());
            }
        }
        Err(error) => {
            state.status_message = Some(terminal_control_error_text(&error));
        }
    }
}

fn handle_terminal_mouse(mouse: MouseEvent, state: &mut AppState) {
    let fleet = state.terminal.clone();
    match state.terminal_ui.forward_mouse(mouse, &fleet) {
        Ok(true) => return,
        Ok(false) => {}
        Err(error) => {
            state.status_message = Some(terminal_control_error_text(&error));
            return;
        }
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(hit) = state.terminal_ui.clicked(mouse.column, mouse.row) else {
                return;
            };
            match hit {
                TerminalHit::New => {
                    let fleet = state.terminal.clone();
                    let result = state.terminal_ui.create(&fleet);
                    report_terminal_action(result, state);
                }
                TerminalHit::Stop => {
                    let result = state.terminal_ui.stop();
                    report_terminal_action(result, state);
                }
                TerminalHit::Close => {
                    let result = state.terminal_ui.close();
                    report_terminal_action(result, state);
                }
                TerminalHit::Latest => {
                    let result = state.terminal_ui.jump_to_latest();
                    report_terminal_action(result, state);
                }
                TerminalHit::Session(id) => {
                    state.terminal_ui.select(id);
                    state.status_message = Some(format!(
                        "{} {id}: {}",
                        i18n::text(Text::Terminal),
                        i18n::text(Text::TerminalSelectedNotice)
                    ));
                }
                TerminalHit::Screen => state.terminal_ui.focus_input(),
            }
        }
        MouseEventKind::ScrollUp => {
            if let Err(error) = state.terminal_ui.scroll(-3) {
                state.status_message = Some(terminal_control_error_text(&error));
            }
        }
        MouseEventKind::ScrollDown => {
            if let Err(error) = state.terminal_ui.scroll(3) {
                state.status_message = Some(terminal_control_error_text(&error));
            }
        }
        _ => {}
    }
}

fn terminal_shortcut(key: KeyEvent, character: char) -> bool {
    key.modifiers
        .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        && matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&character))
}

fn report_terminal_action(
    result: Result<String, crate::terminal::TerminalControlError>,
    state: &mut AppState,
) {
    state.status_message = Some(match result {
        Ok(message) => message,
        Err(error) => terminal_control_error_text(&error),
    });
}

fn handle_tool_menu_action(action_id: u64, action: &str, state: &mut AppState) {
    state.selected_tool = Some(action_id);
    match action {
        "toggle_details" => {
            if !state.expanded_tools.remove(&action_id) {
                state.expanded_tools.insert(action_id);
            }
        }
        "open_chat" => {
            state.shell_ui.select_tab(ShellTab::Chat);
            state.expanded_tools.insert(action_id);
        }
        "open_diff" => {
            state.shell_ui.select_tab(ShellTab::Diff);
        }
        "mention_output" => {
            let content = state.history.iter().find_map(|entry| match &entry.kind {
                crate::agent::state::HistoryKind::ToolResult {
                    action_id: entry_action,
                    ..
                } if *entry_action == action_id => Some(entry.content.clone()),
                _ => None,
            });
            if let Some(content) = content {
                let preview = content.chars().take(500).collect::<String>();
                insert_text(
                    state,
                    &format!(
                        "{} #{action_id} · {}: {preview} ",
                        i18n::text(Text::ToolLabel),
                        i18n::text(Text::OutputLabel)
                    ),
                );
            }
        }
        _ => {}
    }
}

fn handle_menu_action(
    action: &str,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    match action {
        "new_session" => start_new_session(state, command_tx),
        "sessions" => open_sessions(state, command_tx),
        "rewind" => open_rewind(state),
        "quit" => interrupt_or_quit(state, command_tx, urgent_control),
        "tab_chat" => state.shell_ui.select_tab(ShellTab::Chat),
        "tab_activity" => state.shell_ui.select_tab(ShellTab::Activity),
        "tab_diff" => state.shell_ui.select_tab(ShellTab::Diff),
        "tab_plan" => state.shell_ui.select_tab(ShellTab::Plan),
        "tab_agents" => state.shell_ui.select_tab(ShellTab::Agents),
        "tab_terminal" => state.shell_ui.select_tab(ShellTab::Terminal),
        "terminal_new" => {
            state.shell_ui.select_tab(ShellTab::Terminal);
            let fleet = state.terminal.clone();
            let result = state.terminal_ui.create(&fleet);
            report_terminal_action(result, state);
        }
        "terminal_stop" => {
            let result = state.terminal_ui.stop();
            report_terminal_action(result, state);
        }
        "terminal_close" => {
            let result = state.terminal_ui.close();
            report_terminal_action(result, state);
        }
        "toggle_left" => state.shell_ui.show_left_sidebar = !state.shell_ui.show_left_sidebar,
        "toggle_right" => state.shell_ui.show_right_sidebar = !state.shell_ui.show_right_sidebar,
        "toggle_pixel" => {
            let enabled = !state.mascot.enabled();
            match crate::onboarding::persist_mascot_preference(state.language, true, enabled) {
                Ok(_) => {
                    state.mascot.set_enabled(enabled);
                    state.status_message = Some(format!(
                        "{}: {}",
                        i18n::text(Text::PixelDisplay),
                        if enabled {
                            i18n::text(Text::OnLabel)
                        } else {
                            i18n::text(Text::OffLabel)
                        }
                    ));
                }
                Err(error) => {
                    state.status_message = Some(format!("{}: {error}", i18n::text(Text::Failed)));
                }
            }
        }
        "toggle_thinking" => {
            state.show_thinking = !state.show_thinking;
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::ThinkingDisplay),
                if state.show_thinking {
                    i18n::text(Text::OnLabel)
                } else {
                    i18n::text(Text::OffLabel)
                }
            ));
        }
        "toggle_tool_activity" => {
            state.show_tool_activity = !state.show_tool_activity;
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::ToolActivityDisplay),
                if state.show_tool_activity {
                    i18n::text(Text::OnLabel)
                } else {
                    i18n::text(Text::OffLabel)
                }
            ));
        }
        "jump_latest" if state.shell_ui.active_tab() == ShellTab::Terminal => {
            let result = state.terminal_ui.jump_to_latest();
            report_terminal_action(result, state);
        }
        "jump_latest" => jump_to_latest(state),
        "interrupt" => interrupt_or_quit(state, command_tx, urgent_control),
        "runtime" => {
            open_runtime(state);
        }
        "mcp" => open_mcp(state),
        "lsp" => open_lsp(state, command_tx),
        "code_index" => open_code_index(state, command_tx),
        "privacy" => open_privacy(state),
        "permissions" => open_permissions(state),
        "auto_approval" => open_auto_approval(state),
        "usage" => open_usage(state),
        "language" => open_language(state),
        "rerun_setup" => match crate::onboarding::persist_ui_preferences(i18n::current(), false) {
            Ok(_) => {
                state.status_message = Some(i18n::text(Text::SetupNextLaunch).to_owned());
                interrupt_or_quit(state, command_tx, urgent_control);
            }
            Err(error) => {
                state.status_message = Some(format!(
                    "{}: {error}",
                    i18n::text(Text::SetupScheduleFailed)
                ));
            }
        },
        "notifications" => open_notifications(state),
        "github" => open_github(state),
        "reviews" => open_reviews(state),
        "side_chat" => open_side_chat(state),
        "follow_ups" => open_follow_ups(state, false),
        "modes" => open_modes(state),
        "instructions" => open_instructions(state),
        "skills" => open_skills(state),
        "plugins" => open_plugins(state),
        "automation" => open_automation(state),
        "palette" => {
            open_palette(PaletteMode::Commands, state);
        }
        "shortcuts" => {
            state.status_message = Some(i18n::text(Text::ShortcutSummary).to_owned());
        }
        _ => {}
    }
}

fn open_palette(mode: PaletteMode, state: &mut AppState) {
    let opened = match mode {
        PaletteMode::Commands => {
            let total = command_matches(state.automation.commands.as_ref(), "").len();
            state.palette_ui.open(mode, total);
            true
        }
        PaletteMode::Files => match state.palette_ui.open_files(&state.workspace_root) {
            Ok(()) => true,
            Err(error) => {
                state.status_message =
                    Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
                false
            }
        },
        PaletteMode::Closed => {
            state.palette_ui.open(mode, 0);
            true
        }
    };
    if !opened {
        return;
    }
    state.status_message = Some(match mode {
        PaletteMode::Commands => format!(
            "{}: {}",
            i18n::text(Text::CommandPalette),
            i18n::text(Text::OpenedStatus)
        ),
        PaletteMode::Files => format!(
            "{}: {}",
            i18n::text(Text::AttachComputerFile),
            i18n::text(Text::OpenedStatus)
        ),
        PaletteMode::Closed => i18n::text(Text::PaletteClosed).to_owned(),
    });
}

fn open_runtime(state: &mut AppState) {
    if !matches!(
        state.phase,
        crate::agent::phase::AgentPhase::Idle
            | crate::agent::phase::AgentPhase::Error {
                recoverable: true,
                ..
            }
    ) {
        state.status_message = Some(i18n::text(Text::RuntimeChangeIdleOnly).to_owned());
        return;
    }
    if state.deployment_choices.is_empty() {
        state.status_message = Some(i18n::text(Text::NoDeploymentChoices).to_owned());
        return;
    }
    state.runtime_ui.open(
        state.deployment_choices.as_ref(),
        &state.deployment,
        state.reasoning_effort,
        state.work_modes.deep_thinking,
        state.context_budget,
        state.max_context_budget,
    );
    state.status_message = Some(i18n::text(Text::ChooseRuntimeSettings).to_owned());
}

fn open_mcp(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::McpIdleOnly).to_owned());
        return;
    }
    state.mcp_ui.open(state.mcp_servers.len());
    state.mcp_ui.sync(state.mcp_servers.as_ref());
    state.status_message = Some(if state.mcp_servers.is_empty() {
        i18n::text(Text::NoMcpServersAdd).to_owned()
    } else {
        i18n::text(Text::McpManagerOpened).to_owned()
    });
}

fn open_lsp(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state
        .lsp_ui
        .open(state.lsp_servers.len(), state.lsp_diagnostics.len());
    sync_lsp_selection(state);
    if !try_send(
        command_tx,
        OrchestratorCommand::LspRefresh {
            scope: current_scope(state),
        },
        state,
    ) {
        return;
    }
    state.status_message = Some(if state.lsp_servers.is_empty() {
        i18n::text(Text::NoLanguageServers).to_owned()
    } else {
        format!(
            "{}: {}",
            i18n::text(Text::LanguageIntelligence),
            i18n::text(Text::OpenedStatus)
        )
    });
}

fn open_code_index(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.code_index_ui.open(state.code_index_hits.len());
    if !try_send(
        command_tx,
        OrchestratorCommand::CodeIndexPoll {
            scope: current_scope(state),
        },
        state,
    ) {
        return;
    }
    state.status_message = Some(if state.code_index.runtime_available {
        format!(
            "{}: {}",
            i18n::text(Text::RepositoryIntelligence),
            i18n::text(Text::OpenedStatus)
        )
    } else {
        i18n::text(Text::Unavailable).to_owned()
    });
}

fn open_privacy(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.privacy_ui.open(state.privacy.sources.len());
    state.status_message = Some(format!(
        "{}: {}",
        i18n::text(Text::PrivacyShield),
        i18n::text(Text::OpenedStatus)
    ));
}

fn open_permissions(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state
        .permission_ui
        .open(state.shell_permissions.grants.len());
    state.status_message = Some(i18n::text(Text::SessionGrantHelp).to_owned());
}

fn open_usage(state: &mut AppState) {
    state.usage_ui.sync(&state.usage);
    state.usage_ui.open(state.usage.deployments.len());
    state.status_message = Some(match state.usage.cost_coverage() {
        CostCoverage::NoUsage => i18n::text(Text::UsageNoBilled).to_owned(),
        CostCoverage::Unpriced => i18n::text(Text::UsageTariffMissing).to_owned(),
        CostCoverage::Partial => i18n::text(Text::UsagePartialUnpriced).to_owned(),
        CostCoverage::Complete => format!(
            "{}: {}",
            i18n::text(Text::UsageDialogTitle),
            i18n::text(Text::OpenedStatus)
        ),
    });
}

fn open_language(state: &mut AppState) {
    state.language_ui.open(state.language);
    state.status_message = Some(format!(
        "{}: {}",
        i18n::text(Text::InterfaceLanguage),
        i18n::text(Text::OpenedStatus)
    ));
}

fn open_notifications(state: &mut AppState) {
    state.notification_ui.open(&state.notifications);
    state.status_message = Some(format!(
        "{}: {} · {}",
        i18n::text(Text::Notifications),
        i18n::text(Text::OpenedStatus),
        state.notifications.unread_count()
    ));
}

fn open_github(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.github_ui.open(&state.github);
    state.status_message = Some(if state.github.enabled {
        format!(
            "{}: {}",
            i18n::text(Text::GithubPullRequests),
            i18n::text(Text::OpenedStatus)
        )
    } else {
        i18n::text(Text::Unavailable).to_owned()
    });
}

fn open_reviews(state: &mut AppState) {
    state.review_ui.open(&state.reviews);
    state.status_message = Some(if state.reviews.reports.is_empty() {
        i18n::text(Text::NoStructuredReviewReport).to_owned()
    } else {
        format!(
            "{}: {} · {}/{}",
            i18n::text(Text::StructuredCodeReviews),
            i18n::text(Text::OpenedStatus),
            state.reviews.reports.len(),
            state.reviews.open_findings()
        )
    });
}

fn open_side_chat(state: &mut AppState) {
    if state.deployment_choices.is_empty() {
        state.status_message = Some(i18n::text(Text::NoDeploymentChoices).to_owned());
        return;
    }
    state.side_chat_ui.open(
        &state.side_chat,
        state.deployment_choices.as_ref(),
        &state.deployment,
        state.reasoning_effort,
    );
    state.status_message = Some(i18n::text(Text::SideQuestionsSeparate).to_owned());
}

fn open_follow_ups(state: &mut AppState, compose: bool) {
    state.follow_up_ui.open(&state.follow_ups, compose);
    state.status_message = Some(i18n::text(Text::QueueSteerHelp).to_owned());
}

fn open_modes(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::WorkModesIdleOnly).to_owned());
        return;
    }
    state.modes_ui.open(&state.work_modes);
    state.status_message = Some(format!(
        "{}: {}",
        i18n::text(Text::WorkModes),
        i18n::text(Text::OpenedStatus)
    ));
}

fn open_instructions(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.instructions_ui.open(state.instructions.sources.len());
    state.status_message = Some(format!(
        "{}: {}",
        i18n::text(Text::RepositoryInstructions),
        i18n::text(Text::OpenedStatus)
    ));
}

fn open_skills(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.skills_ui.open(state.skills.skills.len());
    state.status_message = Some(if state.skills.skills.is_empty() {
        i18n::text(Text::NoValidSkills).to_owned()
    } else {
        i18n::text(Text::SkillMetadataHelp).to_owned()
    });
}

fn open_automation(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.automation_ui.open(&state.automation);
    state.status_message = Some(format!(
        "{} · {}: {} · {}: {}",
        i18n::text(Text::DefinitionsLoadedClean),
        i18n::text(Text::CustomCommands),
        state.automation.commands.len(),
        i18n::text(Text::LifecycleHooks),
        state.automation.hooks.len()
    ));
}

fn open_plugins(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.plugin_ui.open(&state.plugins);
    state.status_message = Some(format!(
        "{}: {} · {}/{}",
        i18n::text(Text::PluginManager),
        i18n::text(Text::OpenedStatus),
        state.plugins.plugins.len(),
        state.plugins.marketplaces.len()
    ));
}

fn open_auto_approval(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    state.approval_center_ui.open();
    state.status_message = Some(format!(
        "{}: {} · {}/8",
        i18n::text(Text::AutoApprovalCenter),
        i18n::text(Text::OpenedStatus),
        state.auto_approval.enabled_count()
    ));
}

fn handle_approval_center_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.approval_center_ui.close(),
        KeyCode::Tab | KeyCode::Down => state.approval_center_ui.next_focus(),
        KeyCode::BackTab | KeyCode::Up => state.approval_center_ui.previous_focus(),
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(hit) = state.approval_center_ui.focused() {
                handle_approval_center_hit(hit, state, command_tx);
            }
        }
        _ => {}
    }
}

fn handle_approval_center_hit(
    hit: ApprovalHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.approval_center_ui.focus(hit);
    if hit == ApprovalFocus::Close {
        state.approval_center_ui.close();
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::AutoApprovalCenter),
            i18n::text(Text::ClosedStatus)
        ));
        return;
    }
    let mut policy = state.auto_approval;
    match hit {
        ApprovalFocus::All => policy.set_all(!policy.all_enabled()),
        ApprovalFocus::Plans => policy.plans = !policy.plans,
        ApprovalFocus::Workspace => policy.workspace_changes = !policy.workspace_changes,
        ApprovalFocus::Shell => policy.shell = !policy.shell,
        ApprovalFocus::McpRead => policy.mcp_read_only = !policy.mcp_read_only,
        ApprovalFocus::McpMutating => policy.mcp_mutating = !policy.mcp_mutating,
        ApprovalFocus::Continuations => policy.continuations = !policy.continuations,
        ApprovalFocus::SubagentShell => policy.subagent_shell = !policy.subagent_shell,
        ApprovalFocus::SubagentChanges => policy.subagent_changes = !policy.subagent_changes,
        ApprovalFocus::Close => return,
    }
    let command = OrchestratorCommand::SetAutoApprovalPolicy {
        policy,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.auto_approval = policy;
        state.status_message = Some(format!(
            "{}: {}/8 · {}",
            i18n::text(Text::AutoApprovalCenter),
            policy.enabled_count(),
            i18n::text(Text::ApprovalRiskNote)
        ));
    }
}

fn handle_plugin_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if state.plugin_ui.focused() == Some(PluginFocus::Input) {
        match key.code {
            KeyCode::Esc => state.plugin_ui.close(),
            KeyCode::Tab => state.plugin_ui.next_focus(),
            KeyCode::BackTab => state.plugin_ui.previous_focus(),
            KeyCode::Backspace => state.plugin_ui.pop_input(),
            KeyCode::Char(character) => state.plugin_ui.push_input(character),
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc => state.plugin_ui.close(),
        KeyCode::Tab => state.plugin_ui.next_focus(),
        KeyCode::BackTab => state.plugin_ui.previous_focus(),
        KeyCode::Left => state.plugin_ui.focus(PluginFocus::Installed),
        KeyCode::Right => state.plugin_ui.focus(PluginFocus::Marketplace),
        KeyCode::Up => state.plugin_ui.move_selection(&state.plugins, false),
        KeyCode::Down => state.plugin_ui.move_selection(&state.plugins, true),
        KeyCode::Enter | KeyCode::Char(' ') => {
            let hit = match state.plugin_ui.focused() {
                Some(PluginFocus::Installed) => {
                    PluginHit::Installed(state.plugin_ui.selected_installed())
                }
                Some(PluginFocus::Marketplace) => PluginHit::Primary,
                Some(PluginFocus::Input) => PluginHit::Input,
                Some(PluginFocus::AddSource) => PluginHit::AddSource,
                Some(PluginFocus::InstallLocal) => PluginHit::InstallLocal,
                Some(PluginFocus::Refresh) => PluginHit::Refresh,
                Some(PluginFocus::Primary) => PluginHit::Primary,
                Some(PluginFocus::Remove) => PluginHit::Remove,
                Some(PluginFocus::RemoveSource) => PluginHit::RemoveSource,
                Some(PluginFocus::Close) => PluginHit::Close,
                None => return,
            };
            handle_plugin_hit(hit, state, command_tx);
        }
        _ => {}
    }
}

fn handle_plugin_hit(
    hit: PluginHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.plugin_ui.focus_hit(hit);
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
        && matches!(
            hit,
            PluginHit::Installed(_)
                | PluginHit::AddSource
                | PluginHit::InstallLocal
                | PluginHit::Refresh
                | PluginHit::Primary
                | PluginHit::Remove
                | PluginHit::RemoveSource
        )
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    match hit {
        PluginHit::Installed(index) => toggle_plugin(index, state, command_tx),
        PluginHit::Marketplace(_) | PluginHit::Input => {}
        PluginHit::AddSource => {
            let source = state.plugin_ui.input().trim().to_owned();
            if !state.plugin_ui.input_has_visible_text() {
                state.status_message = Some(i18n::text(Text::MarketplaceSourceInput).to_owned());
                state.plugin_ui.focus(PluginFocus::Input);
                return;
            }
            let command = OrchestratorCommand::AddPluginMarketplace {
                source,
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.plugin_ui.clear_input();
                state.status_message = Some(i18n::text(Text::AddingPluginMarketplace).to_owned());
            }
        }
        PluginHit::InstallLocal => {
            let package = state.plugin_ui.input().trim().to_owned();
            if !state.plugin_ui.input_has_visible_text() {
                state.status_message = Some(i18n::text(Text::InstallLocal).to_owned());
                state.plugin_ui.focus(PluginFocus::Input);
                return;
            }
            let command = OrchestratorCommand::InstallLocalPlugin {
                package,
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.plugin_ui.clear_input();
                state.status_message = Some(i18n::text(Text::InstallSelected).to_owned());
            }
        }
        PluginHit::Refresh => {
            let command = OrchestratorCommand::RefreshPlugins {
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message =
                    Some(i18n::text(Text::RefreshingPluginMarketplaces).to_owned());
            }
        }
        PluginHit::Primary => plugin_primary(state, command_tx),
        PluginHit::Remove => {
            let Some(plugin) = state
                .plugins
                .plugins
                .get(state.plugin_ui.selected_installed())
            else {
                state.status_message = Some(i18n::text(Text::SelectedPluginUnavailable).to_owned());
                return;
            };
            let id = plugin.id.clone();
            let command = OrchestratorCommand::RemovePlugin {
                id: id.clone(),
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(format!("{}: {id}", i18n::text(Text::RemovePlugin)));
            }
        }
        PluginHit::RemoveSource => {
            let Some(source) =
                marketplace_source(&state.plugins, state.plugin_ui.selected_marketplace())
            else {
                state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            };
            let source = source.to_owned();
            let command = OrchestratorCommand::RemovePluginMarketplace {
                source: source.clone(),
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message =
                    Some(format!("{}: {source}", i18n::text(Text::RemoveSource)));
            }
        }
        PluginHit::Close => {
            state.plugin_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::PluginManager),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn plugin_primary(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if state.plugin_ui.marketplace_active() {
        let Some((_, plugin)) =
            marketplace_entry(&state.plugins, state.plugin_ui.selected_marketplace())
        else {
            state.status_message = Some(i18n::text(Text::SelectedPluginUnavailable).to_owned());
            return;
        };
        let id = plugin.id.clone();
        let command = OrchestratorCommand::InstallMarketplacePlugin {
            id: id.clone(),
            scope: current_scope(state),
        };
        if try_send(command_tx, command, state) {
            state.status_message = Some(format!("{}: {id}", i18n::text(Text::InstallSelected)));
        }
        return;
    }
    let Some(plugin) = state
        .plugins
        .plugins
        .get(state.plugin_ui.selected_installed())
    else {
        state.status_message = Some(i18n::text(Text::SelectedPluginUnavailable).to_owned());
        return;
    };
    if plugin.update.is_some() {
        let id = plugin.id.clone();
        let command = OrchestratorCommand::UpdatePlugin {
            id: id.clone(),
            scope: current_scope(state),
        };
        if try_send(command_tx, command, state) {
            state.status_message = Some(format!("{}: {id}", i18n::text(Text::UpdateSelected)));
        }
    } else {
        toggle_plugin(state.plugin_ui.selected_installed(), state, command_tx);
    }
}

fn toggle_plugin(
    index: usize,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some(plugin) = state.plugins.plugins.get(index) else {
        state.status_message = Some(i18n::text(Text::SelectedPluginUnavailable).to_owned());
        return;
    };
    let id = plugin.id.clone();
    let enabled = !plugin.enabled;
    let command = OrchestratorCommand::SetPluginEnabled {
        id: id.clone(),
        enabled,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "{}: {id}",
            if enabled {
                i18n::text(Text::EnabledLabel)
            } else {
                i18n::text(Text::DisabledLabel)
            }
        ));
    }
}

fn handle_automation_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let total = automation_item_count(&state.automation, state.automation_ui.pane());
    match key.code {
        KeyCode::Esc => state.automation_ui.close(),
        KeyCode::Tab => state.automation_ui.next_focus(),
        KeyCode::BackTab => state.automation_ui.previous_focus(),
        KeyCode::Left => {
            state
                .automation_ui
                .set_pane(AutomationPane::Commands, state.automation.commands.len());
        }
        KeyCode::Right => {
            state
                .automation_ui
                .set_pane(AutomationPane::Hooks, state.automation.hooks.len());
        }
        KeyCode::Up if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.previous_item(total);
        }
        KeyCode::Down if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.next_item(total);
        }
        KeyCode::Home if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.first_item(total);
        }
        KeyCode::End if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.last_item(total);
        }
        KeyCode::PageUp if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.page_items(total, false);
        }
        KeyCode::PageDown if state.automation_ui.focused() == Some(AutomationFocus::Items) => {
            state.automation_ui.page_items(total, true);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.automation_ui.focused() {
            Some(AutomationFocus::Commands) => {
                handle_automation_hit(AutomationHit::Commands, state, command_tx)
            }
            Some(AutomationFocus::Hooks) => {
                handle_automation_hit(AutomationHit::Hooks, state, command_tx)
            }
            Some(AutomationFocus::Items | AutomationFocus::Primary) => {
                handle_automation_hit(AutomationHit::Primary, state, command_tx)
            }
            Some(AutomationFocus::Reload) => {
                handle_automation_hit(AutomationHit::Reload, state, command_tx)
            }
            Some(AutomationFocus::Close) => {
                handle_automation_hit(AutomationHit::Close, state, command_tx)
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_automation_hit(
    hit: AutomationHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.automation_ui.focus_hit(hit);
    match hit {
        AutomationHit::Commands => state
            .automation_ui
            .set_pane(AutomationPane::Commands, state.automation.commands.len()),
        AutomationHit::Hooks => state
            .automation_ui
            .set_pane(AutomationPane::Hooks, state.automation.hooks.len()),
        AutomationHit::Item(_) => {}
        AutomationHit::ToggleHook(index) => toggle_hook(index, state, command_tx),
        AutomationHit::Primary => match state.automation_ui.pane() {
            AutomationPane::Commands => insert_selected_custom_command(state),
            AutomationPane::Hooks => {
                let index = state.automation_ui.selected();
                toggle_hook(index, state, command_tx);
            }
        },
        AutomationHit::Reload => {
            let command = OrchestratorCommand::ReloadAutomation {
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(i18n::text(Text::ReloadToml).to_owned());
            }
        }
        AutomationHit::Close => {
            state.automation_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::Automation),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn insert_selected_custom_command(state: &mut AppState) {
    let Some(command) = state
        .automation
        .commands
        .get(state.automation_ui.selected())
        .cloned()
    else {
        state.status_message = Some(i18n::text(Text::SelectCustomCommand).to_owned());
        return;
    };
    state.automation_ui.close();
    insert_text(state, &format!("/{} ", command.id));
    state.status_message = Some(if command.argument_hint.is_empty() {
        format!("/{} · {}", command.id, i18n::text(Text::Ready))
    } else {
        format!(
            "/{} · {} · {}: {}",
            command.id,
            i18n::text(Text::Ready),
            i18n::text(Text::ArgumentsPerLine),
            command.argument_hint
        )
    });
}

fn toggle_hook(index: usize, state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let Some(hook) = state.automation.hooks.get(index) else {
        state.status_message = Some(i18n::text(Text::SelectedHookUnavailable).to_owned());
        return;
    };
    let hook_id = hook.id.clone();
    let hook_name = hook.name.clone();
    let was_enabled = hook.enabled;
    let command = OrchestratorCommand::SetHookEnabled {
        id: hook_id,
        enabled: !was_enabled,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "{}: {}",
            if was_enabled {
                i18n::text(Text::DisabledLabel)
            } else {
                i18n::text(Text::EnabledLabel)
            },
            hook_name
        ));
    }
}

fn handle_instructions_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.instructions_ui.close(),
        KeyCode::Tab => state.instructions_ui.next_focus(),
        KeyCode::BackTab => state.instructions_ui.previous_focus(),
        KeyCode::Up if state.instructions_ui.focused() == Some(InstructionsFocus::Sources) => {
            state
                .instructions_ui
                .previous_source(state.instructions.sources.len());
        }
        KeyCode::Down if state.instructions_ui.focused() == Some(InstructionsFocus::Sources) => {
            state
                .instructions_ui
                .next_source(state.instructions.sources.len());
        }
        KeyCode::PageUp if state.instructions_ui.focused() == Some(InstructionsFocus::Sources) => {
            state
                .instructions_ui
                .page_sources(state.instructions.sources.len(), false);
        }
        KeyCode::PageDown
            if state.instructions_ui.focused() == Some(InstructionsFocus::Sources) =>
        {
            state
                .instructions_ui
                .page_sources(state.instructions.sources.len(), true);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.instructions_ui.focused() {
            Some(InstructionsFocus::Global) => {
                handle_instructions_hit(InstructionsHit::Global, state, command_tx)
            }
            Some(InstructionsFocus::Sources) => handle_instructions_hit(
                InstructionsHit::Source(state.instructions_ui.selected()),
                state,
                command_tx,
            ),
            Some(InstructionsFocus::Reload) => {
                handle_instructions_hit(InstructionsHit::Reload, state, command_tx)
            }
            Some(InstructionsFocus::Close) => {
                handle_instructions_hit(InstructionsHit::Close, state, command_tx)
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_instructions_hit(
    hit: InstructionsHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.instructions_ui.focus_hit(hit);
    if !matches!(hit, InstructionsHit::Close)
        && !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    let command = match hit {
        InstructionsHit::Close => {
            state.instructions_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::RepositoryInstructions),
                i18n::text(Text::ClosedStatus)
            ));
            return;
        }
        InstructionsHit::Reload => OrchestratorCommand::ReloadProjectInstructions {
            scope: current_scope(state),
        },
        InstructionsHit::Global => OrchestratorCommand::SetProjectInstructionsEnabled {
            enabled: !state.instructions.project_enabled,
            scope: current_scope(state),
        },
        InstructionsHit::Source(index) => {
            let Some(source) = state.instructions.sources.get(index) else {
                state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            };
            if source.locked {
                state.status_message = Some(i18n::text(Text::TrustedSystemOrigin).to_owned());
                return;
            }
            if !state.instructions.project_enabled {
                state.status_message = Some(i18n::text(Text::EnableInstructionsFirst).to_owned());
                return;
            }
            OrchestratorCommand::SetInstructionSourceEnabled {
                id: source.id.clone(),
                enabled: !source.enabled,
                scope: current_scope(state),
            }
        }
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::RepositoryInstructions),
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn handle_skills_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.skills_ui.close(),
        KeyCode::Tab => state.skills_ui.next_focus(),
        KeyCode::BackTab => state.skills_ui.previous_focus(),
        KeyCode::Up if state.skills_ui.focused() == Some(SkillsFocus::Skills) => {
            state.skills_ui.previous_skill(state.skills.skills.len());
        }
        KeyCode::Down if state.skills_ui.focused() == Some(SkillsFocus::Skills) => {
            state.skills_ui.next_skill(state.skills.skills.len());
        }
        KeyCode::PageUp if state.skills_ui.focused() == Some(SkillsFocus::Skills) => {
            state
                .skills_ui
                .page_skills(state.skills.skills.len(), false);
        }
        KeyCode::PageDown if state.skills_ui.focused() == Some(SkillsFocus::Skills) => {
            state.skills_ui.page_skills(state.skills.skills.len(), true);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.skills_ui.focused() {
            Some(SkillsFocus::Skills) => handle_skills_hit(
                SkillsHit::Skill(state.skills_ui.selected()),
                state,
                command_tx,
            ),
            Some(SkillsFocus::Reload) => {
                handle_skills_hit(SkillsHit::Reload, state, command_tx);
            }
            Some(SkillsFocus::Close) => {
                handle_skills_hit(SkillsHit::Close, state, command_tx);
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_skills_hit(
    hit: SkillsHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.skills_ui.focus_hit(hit);
    if !matches!(hit, SkillsHit::Close)
        && !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    let command = match hit {
        SkillsHit::Close => {
            state.skills_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::AgentSkills),
                i18n::text(Text::ClosedStatus)
            ));
            return;
        }
        SkillsHit::Reload => OrchestratorCommand::ReloadSkills {
            scope: current_scope(state),
        },
        SkillsHit::Skill(index) => {
            let Some(skill) = state.skills.skills.get(index) else {
                state.status_message = Some(i18n::text(Text::SelectedSkillUnavailable).to_owned());
                return;
            };
            OrchestratorCommand::SetSkillEnabled {
                id: skill.id.clone(),
                enabled: !skill.enabled,
                scope: current_scope(state),
            }
        }
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::AgentSkills),
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn handle_modes_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if state.modes_ui.is_editing_goal() {
        match key.code {
            KeyCode::Esc => state.modes_ui.cancel_goal_edit(),
            KeyCode::Tab => state.modes_ui.next_focus(),
            KeyCode::BackTab => state.modes_ui.previous_focus(),
            KeyCode::Backspace
                if state.modes_ui.editor_focused() == Some(GoalEditorFocus::Text) =>
            {
                state.modes_ui.pop_goal_char();
            }
            KeyCode::Enter => match state.modes_ui.editor_focused() {
                Some(GoalEditorFocus::Text) => state.modes_ui.goal_newline(),
                Some(GoalEditorFocus::Save) => {
                    handle_modes_hit(ModesHit::SaveGoal, state, command_tx)
                }
                Some(GoalEditorFocus::Cancel) => {
                    handle_modes_hit(ModesHit::CancelGoal, state, command_tx)
                }
                None => {}
            },
            KeyCode::Char(character)
                if state.modes_ui.editor_focused() == Some(GoalEditorFocus::Text)
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                state.modes_ui.push_goal_char(character);
            }
            _ => {}
        }
        return;
    }
    match key.code {
        KeyCode::Esc => state.modes_ui.close(),
        KeyCode::Tab => state.modes_ui.next_focus(),
        KeyCode::BackTab => state.modes_ui.previous_focus(),
        KeyCode::Enter => match state.modes_ui.overview_focused() {
            Some(ModesFocus::Close) => handle_modes_hit(ModesHit::Close, state, command_tx),
            Some(ModesFocus::Plan) => handle_modes_hit(ModesHit::Plan, state, command_tx),
            Some(ModesFocus::Explore) => handle_modes_hit(ModesHit::Explore, state, command_tx),
            Some(ModesFocus::Review) => handle_modes_hit(ModesHit::Review, state, command_tx),
            Some(ModesFocus::Goal) => handle_modes_hit(ModesHit::Goal, state, command_tx),
            Some(ModesFocus::Deep) => handle_modes_hit(ModesHit::Deep, state, command_tx),
            Some(ModesFocus::EditGoal) => handle_modes_hit(ModesHit::EditGoal, state, command_tx),
            None => {}
        },
        _ => {}
    }
}

fn handle_modes_hit(
    hit: ModesHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let was_editing_goal = state.modes_ui.is_editing_goal();
    state.modes_ui.focus_hit(hit);
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
        && matches!(
            hit,
            ModesHit::Plan
                | ModesHit::Explore
                | ModesHit::Review
                | ModesHit::Goal
                | ModesHit::Deep
                | ModesHit::EditGoal
                | ModesHit::SaveGoal
        )
    {
        state.status_message = Some(i18n::text(Text::WorkModesIdleOnly).to_owned());
        return;
    }
    let command = match hit {
        ModesHit::Close => {
            state.modes_ui.close();
            state.status_message = Some(i18n::text(Text::WorkModesClosed).to_owned());
            return;
        }
        ModesHit::Plan => OrchestratorCommand::SetPlanMode {
            enabled: !state.work_modes.plan,
            scope: current_scope(state),
        },
        ModesHit::Explore => OrchestratorCommand::SetExploreMode {
            enabled: !state.work_modes.explore,
            scope: current_scope(state),
        },
        ModesHit::Review => OrchestratorCommand::SetReviewMode {
            enabled: !state.work_modes.review,
            scope: current_scope(state),
        },
        ModesHit::Deep => OrchestratorCommand::SetDeepThinkingMode {
            enabled: !state.work_modes.deep_thinking,
            scope: current_scope(state),
        },
        ModesHit::Goal if state.work_modes.goal_enabled() => OrchestratorCommand::SetGoal {
            objective: None,
            scope: current_scope(state),
        },
        ModesHit::Goal | ModesHit::EditGoal => {
            state.modes_ui.edit_goal(&state.work_modes);
            return;
        }
        ModesHit::GoalText => return,
        ModesHit::SaveGoal => {
            let objective = state.modes_ui.goal_buffer().trim().to_owned();
            if !state.modes_ui.goal_has_visible_text() {
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::Objective),
                    i18n::text(Text::RequiredLabel)
                ));
                return;
            }
            OrchestratorCommand::SetGoal {
                objective: Some(objective),
                scope: current_scope(state),
            }
        }
        ModesHit::CancelGoal => {
            state.modes_ui.cancel_goal_edit();
            return;
        }
    };
    if try_send(command_tx, command, state) {
        if was_editing_goal {
            state.modes_ui.cancel_goal_edit();
        }
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::WorkModes),
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn handle_plan_review_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc if state.plan_approval_ui.is_editing() => {
            state.plan_approval_ui.set_editing(false);
        }
        KeyCode::Esc => handle_plan_review_hit(PlanHit::Reject, state, command_tx),
        KeyCode::Tab => state.plan_approval_ui.next_focus(),
        KeyCode::BackTab => state.plan_approval_ui.previous_focus(),
        KeyCode::Backspace if state.plan_approval_ui.is_editing() => {
            state.plan_approval_ui.pop_char();
        }
        KeyCode::Enter
            if state.plan_approval_ui.is_editing()
                && state.plan_approval_ui.focused() == Some(PlanFocus::Text) =>
        {
            state.plan_approval_ui.newline();
        }
        KeyCode::Enter => match state.plan_approval_ui.focused() {
            Some(PlanFocus::Approve) => handle_plan_review_hit(PlanHit::Approve, state, command_tx),
            Some(PlanFocus::Edit) => handle_plan_review_hit(PlanHit::Edit, state, command_tx),
            Some(PlanFocus::Reject) => handle_plan_review_hit(PlanHit::Reject, state, command_tx),
            Some(PlanFocus::Text) => state.plan_approval_ui.set_editing(true),
            None => {}
        },
        KeyCode::Char(character)
            if state.plan_approval_ui.is_editing()
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.plan_approval_ui.push_char(character);
        }
        _ => {}
    }
}

fn handle_plan_review_hit(
    hit: PlanHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        PlanHit::Text => {
            state.plan_approval_ui.focus(PlanFocus::Text);
            state.plan_approval_ui.set_editing(true);
            return;
        }
        PlanHit::Edit => {
            let editing = !state.plan_approval_ui.is_editing();
            state.plan_approval_ui.set_editing(editing);
            return;
        }
        PlanHit::Approve | PlanHit::Reject => {}
    }
    let Some(pending) = state.pending_plan_review.clone() else {
        return;
    };
    if hit == PlanHit::Approve && !state.plan_approval_ui.plan_has_visible_text() {
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::PlanPreviewTitle),
            i18n::text(Text::RequiredLabel)
        ));
        return;
    }
    let decision = if hit == PlanHit::Approve {
        PlanDecision::Approve {
            plan: state.plan_approval_ui.plan().to_owned(),
        }
    } else {
        PlanDecision::Reject
    };
    let command = OrchestratorCommand::DecidePlan {
        turn_id: pending.review.turn_id,
        review_id: pending.review.review_id,
        decision,
    };
    if try_send(command_tx, command, state) {
        state.pending_plan_review = None;
        state.plan_approval_ui.sync(None);
        state.status_message = Some(if hit == PlanHit::Approve {
            format!(
                "{} · {}",
                i18n::text(Text::ApproveExecute),
                i18n::text(Text::StartingStatus)
            )
        } else {
            i18n::text(Text::Reject).to_owned()
        });
    }
}

fn handle_lsp_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if state.lsp_ui.is_editing() {
        handle_lsp_editor_key(key, state, command_tx);
        return;
    }
    match key.code {
        KeyCode::Esc => handle_lsp_hit(LspHit::Close, state, command_tx),
        KeyCode::Tab => state.lsp_ui.next_focus(),
        KeyCode::BackTab => state.lsp_ui.previous_focus(),
        KeyCode::Left => {
            state.lsp_ui.set_pane(LspPane::Servers);
            state.lsp_ui.focus(LspFocus::Items);
        }
        KeyCode::Right => {
            state.lsp_ui.set_pane(LspPane::Diagnostics);
            state.lsp_ui.focus(LspFocus::Items);
        }
        KeyCode::Up => {
            state.lsp_ui.previous_item();
            sync_lsp_selection(state);
        }
        KeyCode::Down => {
            state.lsp_ui.next_item();
            sync_lsp_selection(state);
        }
        KeyCode::Home => {
            state.lsp_ui.first_item();
            sync_lsp_selection(state);
        }
        KeyCode::End => {
            state.lsp_ui.last_item();
            sync_lsp_selection(state);
        }
        KeyCode::Char(' ')
            if state.lsp_ui.pane() == LspPane::Servers
                && matches!(
                    state.lsp_ui.focused(),
                    Some(LspFocus::Items | LspFocus::Toggle)
                ) =>
        {
            handle_lsp_hit(LspHit::Toggle, state, command_tx);
        }
        KeyCode::Enter => match state.lsp_ui.focused() {
            Some(LspFocus::ServersTab) => {
                handle_lsp_hit(LspHit::ServersTab, state, command_tx);
            }
            Some(LspFocus::DiagnosticsTab) => {
                handle_lsp_hit(LspHit::DiagnosticsTab, state, command_tx);
            }
            Some(LspFocus::Items) => {}
            Some(LspFocus::Close) => handle_lsp_hit(LspHit::Close, state, command_tx),
            Some(LspFocus::Toggle) => handle_lsp_hit(LspHit::Toggle, state, command_tx),
            Some(LspFocus::Primary) => handle_lsp_hit(LspHit::Primary, state, command_tx),
            Some(LspFocus::Stop) => handle_lsp_hit(LspHit::Stop, state, command_tx),
            Some(LspFocus::Refresh) => handle_lsp_hit(LspHit::Refresh, state, command_tx),
            Some(LspFocus::Mention) => handle_lsp_hit(LspHit::Mention, state, command_tx),
            Some(LspFocus::Add) => handle_lsp_hit(LspHit::Add, state, command_tx),
            None => {}
        },
        _ => {}
    }
}

fn handle_code_index_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            handle_code_index_hit(CodeIndexHitRegion::Close, state, command_tx);
        }
        KeyCode::Tab => state.code_index_ui.next_focus(),
        KeyCode::BackTab => state.code_index_ui.previous_focus(),
        KeyCode::Up => {
            state.code_index_ui.previous_result();
            state.code_index_ui.focus(CodeIndexFocus::Results);
        }
        KeyCode::Down => {
            state.code_index_ui.next_result();
            state.code_index_ui.focus(CodeIndexFocus::Results);
        }
        KeyCode::Home => state.code_index_ui.first_result(),
        KeyCode::End => state.code_index_ui.last_result(),
        KeyCode::Backspace if state.code_index_ui.focused() == Some(CodeIndexFocus::Query) => {
            state.code_index_ui.pop_grapheme();
        }
        KeyCode::Char('u')
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && state.code_index_ui.focused() == Some(CodeIndexFocus::Query) =>
        {
            state.code_index_ui.clear_query();
        }
        KeyCode::Enter => match state.code_index_ui.focused() {
            Some(CodeIndexFocus::Query | CodeIndexFocus::Search) => {
                handle_code_index_hit(CodeIndexHitRegion::Search, state, command_tx);
            }
            Some(CodeIndexFocus::Results) => {
                handle_code_index_hit(CodeIndexHitRegion::Mention, state, command_tx);
            }
            Some(CodeIndexFocus::Close) => {
                handle_code_index_hit(CodeIndexHitRegion::Close, state, command_tx);
            }
            Some(CodeIndexFocus::Refresh) => {
                handle_code_index_hit(CodeIndexHitRegion::Refresh, state, command_tx);
            }
            Some(CodeIndexFocus::Rebuild) => {
                handle_code_index_hit(CodeIndexHitRegion::Rebuild, state, command_tx);
            }
            Some(CodeIndexFocus::Cancel) => {
                handle_code_index_hit(CodeIndexHitRegion::Cancel, state, command_tx);
            }
            Some(CodeIndexFocus::Mention) => {
                handle_code_index_hit(CodeIndexHitRegion::Mention, state, command_tx);
            }
            None => {}
        },
        KeyCode::Char(character)
            if state.code_index_ui.focused() == Some(CodeIndexFocus::Query)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.code_index_ui.push_char(character);
        }
        _ => {}
    }
}

fn handle_code_index_hit(
    hit: CodeIndexHitRegion,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        CodeIndexHitRegion::Query => {
            state.code_index_ui.focus(CodeIndexFocus::Query);
            return;
        }
        CodeIndexHitRegion::Result(index) => {
            state.code_index_ui.select_result(index);
            state.code_index_ui.focus(CodeIndexFocus::Results);
            return;
        }
        CodeIndexHitRegion::Close => {
            state.code_index_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::RepositoryIntelligence),
                i18n::text(Text::ClosedStatus)
            ));
            return;
        }
        CodeIndexHitRegion::Mention => {
            let Some(hit) = state
                .code_index_hits
                .get(state.code_index_ui.selected_result())
                .cloned()
            else {
                state.status_message = Some(i18n::text(Text::NoIndexedProjectFiles).to_owned());
                return;
            };
            let symbols = if hit.symbols.is_empty() {
                String::new()
            } else {
                format!(" ({})", hit.symbols.join(", "))
            };
            insert_text(
                state,
                &format!(
                    "@{}:{}-{}{} ",
                    hit.path, hit.start_line, hit.end_line, symbols
                ),
            );
            state.code_index_ui.close();
            state.shell_ui.select_tab(ShellTab::Chat);
            state.status_message = Some(i18n::text(Text::MentionInChat).to_owned());
            return;
        }
        CodeIndexHitRegion::Refresh
        | CodeIndexHitRegion::Rebuild
        | CodeIndexHitRegion::Cancel
        | CodeIndexHitRegion::Search => {}
    }

    let command = match hit {
        CodeIndexHitRegion::Refresh
            if state.code_index.runtime_available
                && state.code_index.state != crate::code_index::CodeIndexState::Building =>
        {
            OrchestratorCommand::CodeIndexRefresh {
                force: false,
                scope: current_scope(state),
            }
        }
        CodeIndexHitRegion::Rebuild
            if state.code_index.runtime_available
                && state.code_index.state != crate::code_index::CodeIndexState::Building =>
        {
            OrchestratorCommand::CodeIndexRefresh {
                force: true,
                scope: current_scope(state),
            }
        }
        CodeIndexHitRegion::Cancel
            if state.code_index.state == crate::code_index::CodeIndexState::Building =>
        {
            OrchestratorCommand::CodeIndexCancel {
                scope: current_scope(state),
            }
        }
        CodeIndexHitRegion::Search
            if state.code_index.state == crate::code_index::CodeIndexState::Ready
                && state.code_index_ui.query_has_visible_text() =>
        {
            OrchestratorCommand::CodeIndexSearch {
                query: state.code_index_ui.query().to_owned(),
                path: None,
                top: 12,
                scope: current_scope(state),
            }
        }
        _ => return,
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(
            match hit {
                CodeIndexHitRegion::Refresh => i18n::text(Text::ReloadCatalog),
                CodeIndexHitRegion::Rebuild => i18n::text(Text::ReloadFiles),
                CodeIndexHitRegion::Cancel => i18n::text(Text::Cancel),
                CodeIndexHitRegion::Search => i18n::text(Text::SearchingRepoIndex),
                _ => i18n::text(Text::RuntimeUpdated),
            }
            .to_owned(),
        );
    }
}

fn handle_privacy_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => handle_privacy_hit(PrivacyHit::Close, state, command_tx),
        KeyCode::Tab => state.privacy_ui.next_focus(),
        KeyCode::BackTab => state.privacy_ui.previous_focus(),
        KeyCode::Up => state.privacy_ui.previous(),
        KeyCode::Down => state.privacy_ui.next(),
        KeyCode::Enter => match state.privacy_ui.focused() {
            Some(PrivacyFocus::Reload) => {
                handle_privacy_hit(PrivacyHit::Reload, state, command_tx);
            }
            Some(PrivacyFocus::Close) => {
                handle_privacy_hit(PrivacyHit::Close, state, command_tx);
            }
            Some(PrivacyFocus::Sources) | None => {}
        },
        _ => {}
    }
}

fn handle_privacy_hit(
    hit: PrivacyHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        PrivacyHit::Source(index) => state.privacy_ui.select(index),
        PrivacyHit::Close => {
            state.privacy_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::PrivacyShield),
                i18n::text(Text::ClosedStatus)
            ));
        }
        PrivacyHit::Reload => {
            if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
                return;
            }
            state.privacy_ui.focus(PrivacyFocus::Reload);
            if try_send(
                command_tx,
                OrchestratorCommand::ReloadPrivacy {
                    scope: current_scope(state),
                },
                state,
            ) {
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::PrivacyShield),
                    i18n::text(Text::UpdatingStatus)
                ));
            }
        }
    }
}

fn handle_permission_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => handle_permission_hit(PermissionHit::Close, state, command_tx),
        KeyCode::Tab => state.permission_ui.next_focus(),
        KeyCode::BackTab => state.permission_ui.previous_focus(),
        KeyCode::Up => state.permission_ui.previous(),
        KeyCode::Down => state.permission_ui.next(),
        KeyCode::Enter => match state.permission_ui.focused() {
            Some(PermissionFocus::Revoke) => {
                handle_permission_hit(PermissionHit::Revoke, state, command_tx);
            }
            Some(PermissionFocus::Clear) => {
                handle_permission_hit(PermissionHit::Clear, state, command_tx);
            }
            Some(PermissionFocus::Close) => {
                handle_permission_hit(PermissionHit::Close, state, command_tx);
            }
            Some(PermissionFocus::Grants) | None => {}
        },
        _ => {}
    }
}

fn handle_permission_hit(
    hit: PermissionHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
        && matches!(hit, PermissionHit::Revoke | PermissionHit::Clear)
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    match hit {
        PermissionHit::Grant(index) => state.permission_ui.select(index),
        PermissionHit::Close => {
            state.permission_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::SessionPermissions),
                i18n::text(Text::ClosedStatus)
            ));
        }
        PermissionHit::Revoke => {
            state.permission_ui.focus(PermissionFocus::Revoke);
            let Some(grant_id) = state
                .shell_permissions
                .grants
                .get(state.permission_ui.selected_index())
                .map(|grant| grant.id)
            else {
                state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            };
            let _ = try_send(
                command_tx,
                OrchestratorCommand::RevokeSessionShellGrant {
                    grant_id,
                    scope: current_scope(state),
                },
                state,
            );
        }
        PermissionHit::Clear => {
            state.permission_ui.focus(PermissionFocus::Clear);
            if state.shell_permissions.grants.is_empty() {
                state.status_message = Some(i18n::text(Text::NoSessionGrantsHelp).to_owned());
                return;
            }
            let _ = try_send(
                command_tx,
                OrchestratorCommand::ClearSessionShellGrants {
                    scope: current_scope(state),
                },
                state,
            );
        }
    }
}

fn handle_usage_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc if state.usage_ui.is_editing() => {
            handle_usage_hit(UsageHit::Cancel, state, command_tx);
        }
        KeyCode::Esc => handle_usage_hit(UsageHit::Close, state, command_tx),
        KeyCode::Tab => state.usage_ui.next_focus(),
        KeyCode::BackTab => state.usage_ui.previous_focus(),
        KeyCode::Up if !state.usage_ui.is_editing() => {
            state.usage_ui.previous_item();
            state.usage_ui.focus(UsageFocus::Deployments);
        }
        KeyCode::Down if !state.usage_ui.is_editing() => {
            state.usage_ui.next_item();
            state.usage_ui.focus(UsageFocus::Deployments);
        }
        KeyCode::Backspace if state.usage_ui.is_editing() => state.usage_ui.pop_rate_char(),
        KeyCode::Char(value) if state.usage_ui.is_editing() => {
            state.usage_ui.push_rate_char(value);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.usage_ui.focused() {
            Some(UsageFocus::Edit) => handle_usage_hit(UsageHit::Edit, state, command_tx),
            Some(UsageFocus::Close) => handle_usage_hit(UsageHit::Close, state, command_tx),
            Some(UsageFocus::Save) => handle_usage_hit(UsageHit::Save, state, command_tx),
            Some(UsageFocus::Remove) => handle_usage_hit(UsageHit::Remove, state, command_tx),
            Some(UsageFocus::Cancel) => handle_usage_hit(UsageHit::Cancel, state, command_tx),
            _ => {}
        },
        _ => {}
    }
}

fn handle_language_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        KeyCode::Esc => handle_language_hit(LanguageHit::Close, state),
        KeyCode::Tab => state.language_ui.next_focus(),
        KeyCode::BackTab => state.language_ui.previous_focus(),
        KeyCode::Up => {
            state.language_ui.previous();
            state.language_ui.focus(LanguageFocus::Languages);
        }
        KeyCode::Down => {
            state.language_ui.next();
            state.language_ui.focus(LanguageFocus::Languages);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.language_ui.focused() {
            Some(LanguageFocus::Languages | LanguageFocus::Apply) => {
                handle_language_hit(LanguageHit::Apply, state);
            }
            Some(LanguageFocus::Close) => handle_language_hit(LanguageHit::Close, state),
            None => {}
        },
        _ => {}
    }
}

fn handle_notification_key(key: KeyEvent, state: &mut AppState) {
    match key.code {
        KeyCode::Esc => handle_notification_hit(NotificationHit::Close, state),
        KeyCode::Tab => state.notification_ui.next_focus(),
        KeyCode::BackTab => state.notification_ui.previous_focus(),
        KeyCode::Up => {
            state.notification_ui.previous_item();
            state.notification_ui.focus(NotificationFocus::Items);
        }
        KeyCode::Down => {
            state.notification_ui.next_item();
            state.notification_ui.focus(NotificationFocus::Items);
        }
        KeyCode::Enter | KeyCode::Char(' ') => match state.notification_ui.focused() {
            Some(NotificationFocus::Items) => handle_notification_hit(
                NotificationHit::Item(state.notification_ui.selected()),
                state,
            ),
            Some(NotificationFocus::ActionBell) => {
                handle_notification_hit(NotificationHit::ActionBell, state)
            }
            Some(NotificationFocus::CompletionBell) => {
                handle_notification_hit(NotificationHit::CompletionBell, state)
            }
            Some(NotificationFocus::ErrorBell) => {
                handle_notification_hit(NotificationHit::ErrorBell, state)
            }
            Some(NotificationFocus::MarkAllRead) => {
                handle_notification_hit(NotificationHit::MarkAllRead, state)
            }
            Some(NotificationFocus::ClearRead) => {
                handle_notification_hit(NotificationHit::ClearRead, state)
            }
            Some(NotificationFocus::Close) => {
                handle_notification_hit(NotificationHit::Close, state)
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_github_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            if matches!(state.github_ui.stage(), GitHubStage::Browse) {
                handle_github_hit(GitHubHit::Close, state, command_tx);
            } else {
                handle_github_hit(GitHubHit::Cancel, state, command_tx);
            }
        }
        KeyCode::Tab => state.github_ui.next_focus(),
        KeyCode::BackTab => state.github_ui.previous_focus(),
        KeyCode::Up => {
            state.github_ui.previous_item();
            state.github_ui.focus(GitHubFocus::PullRequests);
        }
        KeyCode::Down => {
            state.github_ui.next_item();
            state.github_ui.focus(GitHubFocus::PullRequests);
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            let hit = match state.github_ui.focused() {
                Some(GitHubFocus::PullRequests) => {
                    Some(GitHubHit::PullRequest(state.github_ui.selected()))
                }
                Some(GitHubFocus::Refresh) => Some(GitHubHit::Refresh),
                Some(GitHubFocus::Open) => Some(GitHubHit::Open),
                Some(GitHubFocus::Checkout) => Some(GitHubHit::Checkout),
                Some(GitHubFocus::CreateDraft) => Some(GitHubHit::CreateDraft),
                Some(GitHubFocus::Cancel) => Some(GitHubHit::Cancel),
                Some(GitHubFocus::Confirm) => Some(GitHubHit::Confirm),
                Some(GitHubFocus::Close) => Some(GitHubHit::Close),
                None => None,
            };
            if let Some(hit) = hit {
                handle_github_hit(hit, state, command_tx);
            }
        }
        _ => {}
    }
}

fn handle_github_hit(
    hit: GitHubHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        GitHubHit::PullRequest(index) => state.github_ui.select(index),
        GitHubHit::Refresh => {
            if state.github.enabled && !state.github.busy {
                let _ = send_github_command(
                    OrchestratorCommand::GitHubRefresh {
                        scope: current_scope(state),
                    },
                    i18n::text(Text::RefreshingGithubPrs),
                    state,
                    command_tx,
                );
            }
        }
        GitHubHit::Open => {
            let number = state
                .github
                .pull_requests
                .get(state.github_ui.selected())
                .map(|request| request.number);
            if state.github.enabled
                && !state.github.busy
                && let Some(number) = number
            {
                let _ = send_github_command(
                    OrchestratorCommand::GitHubOpen {
                        number,
                        scope: current_scope(state),
                    },
                    i18n::text(Text::OpenPullRequests),
                    state,
                    command_tx,
                );
            }
        }
        GitHubHit::Checkout => {
            let number = state
                .github
                .pull_requests
                .get(state.github_ui.selected())
                .map(|request| request.number);
            if state.github.enabled
                && !state.github.busy
                && let Some(number) = number
            {
                state.github_ui.confirm_checkout(number);
                state.status_message = Some(i18n::text(Text::CheckoutSafetyWarning).to_owned());
            }
        }
        GitHubHit::CreateDraft => {
            if state.github.enabled && !state.github.busy {
                state.github_ui.confirm_create();
                state.status_message = Some(i18n::text(Text::CreateDraftConfirmation).to_owned());
            }
        }
        GitHubHit::Cancel => {
            state.github_ui.back();
            state.status_message = Some(i18n::text(Text::Cancelled).to_owned());
        }
        GitHubHit::Confirm => match state.github_ui.stage() {
            GitHubStage::ConfirmCheckout => {
                let number = state.github_ui.confirming_pull_request().filter(|number| {
                    state.github.enabled
                        && !state.github.busy
                        && state
                            .github
                            .pull_requests
                            .iter()
                            .any(|request| request.number == *number)
                });
                if let Some(number) = number
                    && send_github_command(
                        OrchestratorCommand::GitHubCheckout {
                            number,
                            scope: current_scope(state),
                        },
                        i18n::text(Text::CheckoutPullRequest),
                        state,
                        command_tx,
                    )
                {
                    state.github_ui.back();
                }
            }
            GitHubStage::ConfirmCreate => {
                if state.github.enabled
                    && !state.github.busy
                    && send_github_command(
                        OrchestratorCommand::GitHubCreateDraft {
                            scope: current_scope(state),
                        },
                        i18n::text(Text::CreateDraftLabel),
                        state,
                        command_tx,
                    )
                {
                    state.github_ui.back();
                }
            }
            GitHubStage::Closed | GitHubStage::Browse => {}
        },
        GitHubHit::Close => {
            state.github_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::GithubPullRequests),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn send_github_command(
    command: OrchestratorCommand,
    status: &str,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) -> bool {
    if try_send(command_tx, command, state) {
        state.status_message = Some(status.to_owned());
        true
    } else {
        false
    }
}

fn handle_review_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => handle_review_hit(ReviewHit::Close, state, command_tx),
        KeyCode::Tab => state.review_ui.next_focus(),
        KeyCode::BackTab => state.review_ui.previous_focus(),
        KeyCode::Up => {
            state.review_ui.select_previous_finding();
            state.review_ui.focus_hit(ReviewHit::Finding(
                state
                    .review_ui
                    .finding(&state.reviews)
                    .and_then(|finding| {
                        state.review_ui.report(&state.reviews).and_then(|report| {
                            report
                                .findings
                                .iter()
                                .position(|item| item.id == finding.id)
                        })
                    })
                    .unwrap_or(0),
            ));
        }
        KeyCode::Down => {
            state.review_ui.select_next_finding(&state.reviews);
            let selected = state
                .review_ui
                .finding(&state.reviews)
                .and_then(|finding| {
                    state.review_ui.report(&state.reviews).and_then(|report| {
                        report
                            .findings
                            .iter()
                            .position(|item| item.id == finding.id)
                    })
                })
                .unwrap_or(0);
            state.review_ui.focus_hit(ReviewHit::Finding(selected));
        }
        KeyCode::Left => state.review_ui.previous_report(&state.reviews),
        KeyCode::Right => state.review_ui.next_report(&state.reviews),
        KeyCode::PageUp => state.review_ui.scroll_detail(-8),
        KeyCode::PageDown => state.review_ui.scroll_detail(8),
        KeyCode::Enter => match state.review_ui.focused() {
            Some(ReviewFocus::PreviousReport) => {
                handle_review_hit(ReviewHit::PreviousReport, state, command_tx)
            }
            Some(ReviewFocus::NextReport) => {
                handle_review_hit(ReviewHit::NextReport, state, command_tx)
            }
            Some(ReviewFocus::Accept) => handle_review_hit(ReviewHit::Accept, state, command_tx),
            Some(ReviewFocus::QueueFix) => {
                handle_review_hit(ReviewHit::QueueFix, state, command_tx)
            }
            Some(ReviewFocus::Dismiss) => handle_review_hit(ReviewHit::Dismiss, state, command_tx),
            Some(ReviewFocus::Close) => handle_review_hit(ReviewHit::Close, state, command_tx),
            Some(ReviewFocus::Findings) | None => {}
        },
        _ => {}
    }
}

fn handle_side_chat_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
        submit_side_question(state, command_tx);
        return;
    }
    match key.code {
        KeyCode::Esc => handle_side_chat_hit(SideHit::Close, state, command_tx),
        KeyCode::Tab => state.side_chat_ui.next_focus(),
        KeyCode::BackTab => state.side_chat_ui.previous_focus(),
        KeyCode::Up => state.side_chat_ui.previous_item(),
        KeyCode::Down => state.side_chat_ui.next_item(),
        KeyCode::PageUp => state.side_chat_ui.scroll_answer(-8),
        KeyCode::PageDown => state.side_chat_ui.scroll_answer(8),
        KeyCode::Backspace
            if state.side_chat_ui.stage() == SideStage::Compose
                && state.side_chat_ui.focused() == Some(SideFocus::Question) =>
        {
            state.side_chat_ui.pop_char();
        }
        KeyCode::Enter
            if state.side_chat_ui.stage() == SideStage::Compose
                && state.side_chat_ui.focused() == Some(SideFocus::Question) =>
        {
            state.side_chat_ui.push_char('\n');
        }
        KeyCode::Enter => match state.side_chat_ui.focused() {
            Some(SideFocus::Primary) => handle_side_chat_hit(SideHit::Primary, state, command_tx),
            Some(SideFocus::Secondary) => {
                handle_side_chat_hit(SideHit::Secondary, state, command_tx)
            }
            Some(SideFocus::Close) => handle_side_chat_hit(SideHit::Close, state, command_tx),
            _ => {}
        },
        KeyCode::Char(character)
            if state.side_chat_ui.stage() == SideStage::Compose
                && state.side_chat_ui.focused() == Some(SideFocus::Question)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.side_chat_ui.push_char(character);
        }
        _ => {}
    }
}

fn handle_side_chat_hit(
    hit: SideHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        SideHit::Question => state.side_chat_ui.focus(SideFocus::Question),
        SideHit::Model(index) => state.side_chat_ui.select_model(index),
        SideHit::Effort(index) => state.side_chat_ui.select_effort(index),
        SideHit::History(index) => state.side_chat_ui.select_history(index),
        SideHit::Close => {
            state.side_chat_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::SideQuestion),
                i18n::text(Text::ClosedStatus)
            ));
        }
        SideHit::Secondary => match state.side_chat_ui.stage() {
            SideStage::Compose if !state.side_chat.exchanges.is_empty() => {
                state.side_chat_ui.show_transcript();
            }
            SideStage::Transcript
                if state
                    .side_chat_ui
                    .selected_exchange(&state.side_chat)
                    .is_some_and(|exchange| exchange.status.is_terminal()) =>
            {
                state.side_chat_ui.compose();
            }
            SideStage::Transcript => {
                state.status_message = Some(i18n::text(Text::AlreadyRunning).to_owned());
            }
            SideStage::Closed | SideStage::Compose => {}
        },
        SideHit::Primary => match state.side_chat_ui.stage() {
            SideStage::Compose => submit_side_question(state, command_tx),
            SideStage::Transcript => {
                let selected = state
                    .side_chat_ui
                    .selected_exchange(&state.side_chat)
                    .cloned();
                match selected.map(|exchange| (exchange.status, exchange)) {
                    Some((crate::agent::SideExchangeStatus::Running, exchange)) => {
                        let command = OrchestratorCommand::CancelSideQuestion {
                            request_id: exchange.id,
                            scope: current_scope(state),
                        };
                        if try_send(command_tx, command, state) {
                            state.status_message =
                                Some(i18n::text(Text::CancellingSideQuestion).to_owned());
                        }
                    }
                    Some((crate::agent::SideExchangeStatus::Completed, exchange)) => {
                        promote_side_answer(&exchange, state);
                    }
                    Some((
                        crate::agent::SideExchangeStatus::Failed
                        | crate::agent::SideExchangeStatus::Cancelled,
                        _,
                    ))
                    | None => state.side_chat_ui.compose(),
                }
            }
            SideStage::Closed => {}
        },
    }
}

fn submit_side_question(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let question = state.side_chat_ui.question().trim().to_owned();
    if !crate::agent::side_chat::has_visible_text(&question) {
        state.status_message = Some(i18n::text(Text::VisibleTextRequired).to_owned());
        return;
    }
    if state
        .side_chat
        .latest()
        .is_some_and(|exchange| exchange.status == crate::agent::SideExchangeStatus::Running)
    {
        state.status_message = Some(i18n::text(Text::AlreadyRunning).to_owned());
        return;
    }
    let Some(deployment) = state
        .side_chat_ui
        .selected_model()
        .filter(|selected| {
            state
                .deployment_choices
                .iter()
                .any(|available| available == selected)
        })
        .map(str::to_owned)
    else {
        state.status_message = Some(i18n::text(Text::SelectTrustedSideDeployment).to_owned());
        return;
    };
    let command = OrchestratorCommand::AskSideQuestion {
        question,
        deployment,
        reasoning_effort: state.side_chat_ui.selected_effort(),
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.side_chat_ui.mark_submitted();
        state.status_message = Some(i18n::text(Text::SideQuestionsSeparate).to_owned());
    }
}

fn promote_side_answer(exchange: &crate::agent::SideExchange, state: &mut AppState) {
    let question = crate::ui::render::sanitize_for_display(&exchange.question);
    let answer = crate::ui::render::sanitize_for_display(&exchange.answer);
    insert_text(
        state,
        &format!(
            "[User-promoted provisional side answer #{} from context revision {}; verify before relying on it]\nQuestion: {}\nAnswer: {}\n\n",
            exchange.id, exchange.context_revision, question, answer
        ),
    );
    state.side_chat_ui.close();
    state.shell_ui.select_tab(ShellTab::Chat);
    state.status_message = Some(i18n::text(Text::SideCommittedSnapshotHelp).to_owned());
}

fn handle_follow_up_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => handle_follow_up_hit(FollowUpHit::Close, state, command_tx),
        KeyCode::Tab => state.follow_up_ui.next_focus(),
        KeyCode::BackTab => state.follow_up_ui.previous_focus(),
        KeyCode::Up => state.follow_up_ui.previous_item(),
        KeyCode::Down => state.follow_up_ui.next_item(),
        KeyCode::PageUp => state.follow_up_ui.scroll_detail(-8),
        KeyCode::PageDown => state.follow_up_ui.scroll_detail(8),
        KeyCode::Backspace
            if matches!(
                state.follow_up_ui.stage(),
                FollowUpStage::Compose | FollowUpStage::Edit
            ) && state.follow_up_ui.focused() == Some(FollowUpFocus::Editor) =>
        {
            state.follow_up_ui.pop_char();
        }
        KeyCode::Enter
            if matches!(
                state.follow_up_ui.stage(),
                FollowUpStage::Compose | FollowUpStage::Edit
            ) && state.follow_up_ui.focused() == Some(FollowUpFocus::Editor) =>
        {
            state.follow_up_ui.push_char('\n');
        }
        KeyCode::Enter => activate_follow_up_focus(state, command_tx),
        KeyCode::Char(character)
            if matches!(
                state.follow_up_ui.stage(),
                FollowUpStage::Compose | FollowUpStage::Edit
            ) && state.follow_up_ui.focused() == Some(FollowUpFocus::Editor)
                && !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.follow_up_ui.push_char(character);
        }
        _ => {}
    }
}

fn activate_follow_up_focus(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let hit = match (state.follow_up_ui.stage(), state.follow_up_ui.focused()) {
        (_, Some(FollowUpFocus::Close)) => Some(FollowUpHit::Close),
        (FollowUpStage::Compose, Some(FollowUpFocus::Primary)) => Some(FollowUpHit::Queue),
        (FollowUpStage::Compose, Some(FollowUpFocus::Secondary)) => Some(FollowUpHit::Steer),
        (FollowUpStage::Compose, Some(FollowUpFocus::Tertiary)) => Some(FollowUpHit::Browse),
        (FollowUpStage::Edit, Some(FollowUpFocus::Primary)) => Some(FollowUpHit::Save),
        (FollowUpStage::Browse, Some(FollowUpFocus::Primary)) => {
            match selected_follow_up(state).map(|item| item.status) {
                Some(FollowUpStatus::Pending | FollowUpStatus::Failed) => Some(FollowUpHit::Edit),
                _ => Some(FollowUpHit::New),
            }
        }
        (FollowUpStage::Browse, Some(FollowUpFocus::Secondary)) => Some(FollowUpHit::CancelOrRetry),
        (FollowUpStage::Browse, Some(FollowUpFocus::Tertiary)) => Some(FollowUpHit::RunNext),
        _ => None,
    };
    if let Some(hit) = hit {
        handle_follow_up_hit(hit, state, command_tx);
    }
}

fn handle_follow_up_hit(
    hit: FollowUpHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        FollowUpHit::Editor => state.follow_up_ui.focus(FollowUpFocus::Editor),
        FollowUpHit::Item(index) => state.follow_up_ui.select(index),
        FollowUpHit::Queue => enqueue_follow_up(FollowUpMode::Queue, state, command_tx),
        FollowUpHit::Steer => enqueue_follow_up(FollowUpMode::Steer, state, command_tx),
        FollowUpHit::Browse => state.follow_up_ui.browse(),
        FollowUpHit::New => state.follow_up_ui.compose(),
        FollowUpHit::Edit => {
            if let Some(item) = selected_follow_up(state)
                && matches!(
                    item.status,
                    FollowUpStatus::Pending | FollowUpStatus::Failed
                )
            {
                state.follow_up_ui.begin_edit(&item);
            }
        }
        FollowUpHit::CancelOrRetry => {
            if let Some(item) = selected_follow_up(state) {
                let command = match item.status {
                    FollowUpStatus::Pending => Some(OrchestratorCommand::CancelFollowUp {
                        id: item.id,
                        revision: item.revision,
                        scope: current_scope(state),
                    }),
                    FollowUpStatus::Failed
                        if item.mode == FollowUpMode::Queue || state.phase.is_busy() =>
                    {
                        Some(OrchestratorCommand::RetryFollowUp {
                            id: item.id,
                            revision: item.revision,
                            scope: current_scope(state),
                        })
                    }
                    FollowUpStatus::Failed => None,
                    FollowUpStatus::Dispatching
                    | FollowUpStatus::Delivered
                    | FollowUpStatus::Cancelled => None,
                };
                if let Some(command) = command {
                    let _ = try_send(command_tx, command, state);
                }
            }
        }
        FollowUpHit::RunNext => {
            let can_dispatch = matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
                && selected_follow_up(state).is_some_and(|item| {
                    item.status == FollowUpStatus::Pending && item.mode == FollowUpMode::Queue
                });
            if !can_dispatch {
                return;
            }
            let _ = try_send(
                command_tx,
                OrchestratorCommand::DispatchFollowUpQueue {
                    scope: current_scope(state),
                },
                state,
            );
        }
        FollowUpHit::Save => {
            let text = state.follow_up_ui.editor().trim().to_owned();
            if !state.follow_up_ui.editor_has_visible_text() {
                state.status_message = Some(i18n::text(Text::VisibleTextRequired).to_owned());
                return;
            }
            if let Some((id, revision)) = state.follow_up_ui.editing() {
                let command = OrchestratorCommand::EditFollowUp {
                    id,
                    revision,
                    text,
                    scope: current_scope(state),
                };
                if try_send(command_tx, command, state) {
                    state.follow_up_ui.clear_after_submit();
                }
            }
        }
        FollowUpHit::Close => {
            state.follow_up_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::QueueAndSteer),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn enqueue_follow_up(
    mode: FollowUpMode,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let text = state.follow_up_ui.editor().trim().to_owned();
    if !state.follow_up_ui.editor_has_visible_text() {
        state.status_message = Some(i18n::text(Text::VisibleTextRequired).to_owned());
        return;
    }
    if mode == FollowUpMode::Steer && !state.phase.is_busy() {
        state.status_message = Some(i18n::text(Text::NoActiveTurn).to_owned());
        return;
    }
    let command = OrchestratorCommand::EnqueueFollowUp {
        mode,
        text,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.follow_up_ui.clear_after_submit();
        state.status_message = Some(match mode {
            FollowUpMode::Queue => i18n::text(Text::RunFifoNext).to_owned(),
            FollowUpMode::Steer => i18n::text(Text::SteerSafely).to_owned(),
        });
    }
}

fn selected_follow_up(state: &AppState) -> Option<crate::agent::FollowUpItem> {
    state
        .follow_ups
        .items
        .get(state.follow_up_ui.selected_index())
        .cloned()
}

fn handle_usage_hit(
    hit: UsageHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if matches!(hit, UsageHit::Edit | UsageHit::Save | UsageHit::Remove)
        && !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    match hit {
        UsageHit::Deployment(index) => state.usage_ui.select(index),
        UsageHit::Edit => {
            if let Some(item) = selected_usage_deployment(state) {
                state.usage_ui.begin_edit(&item);
                state.status_message = Some(format!(
                    "{} {}",
                    i18n::text(Text::EditingExactTariff),
                    item.deployment
                ));
            }
        }
        UsageHit::Close => {
            state.usage_ui.close();
            state.status_message = Some(i18n::text(Text::UsagePanelClosed).to_owned());
        }
        UsageHit::InputRate => state.usage_ui.focus(UsageFocus::InputRate),
        UsageHit::CachedRate => state.usage_ui.focus(UsageFocus::CachedRate),
        UsageHit::OutputRate => state.usage_ui.focus(UsageFocus::OutputRate),
        UsageHit::LongThreshold => state.usage_ui.focus(UsageFocus::LongThreshold),
        UsageHit::LongInputRate => state.usage_ui.focus(UsageFocus::LongInputRate),
        UsageHit::LongCachedRate => state.usage_ui.focus(UsageFocus::LongCachedRate),
        UsageHit::LongOutputRate => state.usage_ui.focus(UsageFocus::LongOutputRate),
        UsageHit::Save => {
            let Some(deployment) = selected_usage_deployment(state).map(|item| item.deployment)
            else {
                state
                    .usage_ui
                    .set_editor_error(i18n::text(Text::NoDeploymentSelected).to_owned());
                return;
            };
            match state.usage_ui.build_pricing(&deployment) {
                Ok(pricing) => {
                    if try_send(
                        command_tx,
                        OrchestratorCommand::SetDeploymentPricing {
                            pricing,
                            scope: current_scope(state),
                        },
                        state,
                    ) {
                        state.usage_ui.cancel_edit();
                        state.status_message = Some(format!(
                            "{}: {deployment}",
                            i18n::text(Text::SaveRecalculate)
                        ));
                    }
                }
                Err(error) => state.usage_ui.set_editor_error(error.to_string()),
            }
        }
        UsageHit::Remove => {
            let Some(item) = selected_usage_deployment(state) else {
                state
                    .usage_ui
                    .set_editor_error(i18n::text(Text::NoDeploymentSelected).to_owned());
                return;
            };
            if !item.pricing_provenance.as_ref().is_some_and(|provenance| {
                provenance.source == crate::usage::PricingSource::UserOverride
            }) {
                state
                    .usage_ui
                    .set_editor_error(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            }
            let deployment = item.deployment;
            if try_send(
                command_tx,
                OrchestratorCommand::RemoveDeploymentPricing {
                    deployment: deployment.clone(),
                    scope: current_scope(state),
                },
                state,
            ) {
                state.usage_ui.cancel_edit();
                state.status_message = Some(format!(
                    "{}: {deployment}",
                    i18n::text(Text::RemoveOverride)
                ));
            }
        }
        UsageHit::Cancel => {
            state.usage_ui.cancel_edit();
            state.status_message = Some(i18n::text(Text::Cancelled).to_owned());
        }
    }
}

fn selected_usage_deployment(state: &AppState) -> Option<DeploymentUsageSnapshot> {
    state.usage_ui.selected_deployment().map_or_else(
        || {
            state
                .usage
                .deployments
                .get(state.usage_ui.selected())
                .cloned()
        },
        |deployment| {
            state
                .usage
                .deployments
                .iter()
                .find(|item| item.deployment == deployment)
                .cloned()
        },
    )
}

fn handle_language_hit(hit: LanguageHit, state: &mut AppState) {
    match hit {
        LanguageHit::Language(index) => state.language_ui.select(index),
        LanguageHit::Apply => {
            let language = state.language_ui.selected_language();
            match crate::onboarding::persist_ui_preferences(language, true) {
                Ok(_) => {
                    state.language = language;
                    super::i18n::set_language(language);
                    state.language_ui.close();
                    state.status_message =
                        Some(format!("{} [{}]", language.label(), language.code()));
                }
                Err(error) => {
                    state.status_message = Some(format!("{}: {error}", i18n::text(Text::Failed)));
                }
            }
        }
        LanguageHit::Close => {
            state.language_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::InterfaceLanguage),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn handle_notification_hit(hit: NotificationHit, state: &mut AppState) {
    match hit {
        NotificationHit::Item(index) => {
            state.notification_ui.select(index);
            state.notifications.mark_read(index);
            state.status_message = Some(i18n::text(Text::MarkAllRead).to_owned());
        }
        NotificationHit::ActionBell => {
            state.notifications.toggle_action_bell();
            state.notification_ui.focus(NotificationFocus::ActionBell);
        }
        NotificationHit::CompletionBell => {
            state.notifications.toggle_completion_bell();
            state
                .notification_ui
                .focus(NotificationFocus::CompletionBell);
        }
        NotificationHit::ErrorBell => {
            state.notifications.toggle_error_bell();
            state.notification_ui.focus(NotificationFocus::ErrorBell);
        }
        NotificationHit::MarkAllRead => {
            state.notifications.mark_all_read();
            state.notification_ui.focus(NotificationFocus::MarkAllRead);
            state.status_message = Some(i18n::text(Text::MarkAllRead).to_owned());
        }
        NotificationHit::ClearRead => {
            state.notifications.clear_read();
            state.notification_ui.sync(&state.notifications);
            state.notification_ui.focus(NotificationFocus::ClearRead);
            state.status_message = Some(i18n::text(Text::ClearRead).to_owned());
        }
        NotificationHit::Close => {
            state.notification_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::Notifications),
                i18n::text(Text::ClosedStatus)
            ));
        }
    }
}

fn handle_review_hit(
    hit: ReviewHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.review_ui.focus_hit(hit);
    match hit {
        ReviewHit::PreviousReport => {
            state.review_ui.previous_report(&state.reviews);
        }
        ReviewHit::NextReport => {
            state.review_ui.next_report(&state.reviews);
        }
        ReviewHit::Finding(_) => {}
        ReviewHit::Close => {
            state.review_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::StructuredCodeReviews),
                i18n::text(Text::ClosedStatus)
            ));
        }
        ReviewHit::Accept | ReviewHit::QueueFix | ReviewHit::Dismiss => {
            if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
                state.status_message = Some(i18n::text(Text::ReviewIdleDecisionHelp).to_owned());
                return;
            }
            let Some(report) = state.review_ui.report(&state.reviews) else {
                state.status_message = Some(i18n::text(Text::NoStructuredReviewReport).to_owned());
                return;
            };
            let Some(finding) = state.review_ui.finding(&state.reviews) else {
                state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            };
            if finding.disposition != crate::agent::ReviewFindingDisposition::Open {
                state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
                return;
            }
            let decision = match hit {
                ReviewHit::Accept => ReviewFindingDecision::Accept,
                ReviewHit::QueueFix => ReviewFindingDecision::QueueFix,
                ReviewHit::Dismiss => ReviewFindingDecision::Dismiss,
                ReviewHit::PreviousReport
                | ReviewHit::Finding(_)
                | ReviewHit::NextReport
                | ReviewHit::Close => return,
            };
            let command = OrchestratorCommand::DecideReviewFinding {
                report_id: report.id,
                revision: report.revision,
                finding_id: finding.id,
                decision,
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(match decision {
                    ReviewFindingDecision::Accept => i18n::text(Text::AcceptFinding).to_owned(),
                    ReviewFindingDecision::Dismiss => i18n::text(Text::DismissFinding).to_owned(),
                    ReviewFindingDecision::QueueFix => i18n::text(Text::QueueSafeFix).to_owned(),
                });
            }
        }
    }
}

fn handle_lsp_hit(
    hit: LspHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let pane = state.lsp_ui.pane();
    let wrong_pane = matches!(
        hit,
        LspHit::ServerItem(_) | LspHit::Toggle | LspHit::Primary | LspHit::Stop | LspHit::Add
    ) && pane != LspPane::Servers
        || matches!(hit, LspHit::DiagnosticItem(_) | LspHit::Mention)
            && pane != LspPane::Diagnostics;
    if wrong_pane {
        return;
    }
    if matches!(
        hit,
        LspHit::Refresh | LspHit::Toggle | LspHit::Primary | LspHit::Stop | LspHit::Add
    ) && !matches!(state.phase, crate::agent::phase::AgentPhase::Idle)
    {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    match hit {
        LspHit::ServersTab => {
            state.lsp_ui.set_pane(LspPane::Servers);
            return;
        }
        LspHit::DiagnosticsTab => {
            state.lsp_ui.set_pane(LspPane::Diagnostics);
            return;
        }
        LspHit::ServerItem(index) => {
            state.lsp_ui.select_server(index);
            sync_lsp_selection(state);
            state.lsp_ui.focus(LspFocus::Items);
            return;
        }
        LspHit::DiagnosticItem(index) => {
            state.lsp_ui.select_diagnostic(index);
            sync_lsp_selection(state);
            state.lsp_ui.focus(LspFocus::Items);
            return;
        }
        LspHit::Close => {
            state.lsp_ui.close();
            state.status_message = Some(format!(
                "{}: {}",
                i18n::text(Text::LanguageIntelligence),
                i18n::text(Text::ClosedStatus)
            ));
            return;
        }
        LspHit::Refresh => {
            if try_send(
                command_tx,
                OrchestratorCommand::LspRefresh {
                    scope: current_scope(state),
                },
                state,
            ) {
                state.status_message = Some(i18n::text(Text::RefreshingLspStatus).to_owned());
            }
            return;
        }
        LspHit::Mention => {
            let Some(diagnostic) = state
                .lsp_diagnostics
                .get(state.lsp_ui.selected_diagnostic())
                .cloned()
            else {
                state.status_message = Some(i18n::text(Text::NoDiagnosticsPublished).to_owned());
                return;
            };
            let message = diagnostic.message.chars().take(500).collect::<String>();
            insert_text(
                state,
                &format!(
                    "@{}:{}:{} [{} from {}] {} ",
                    diagnostic.path,
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.severity.label(),
                    diagnostic.server,
                    message
                ),
            );
            state.lsp_ui.close();
            state.shell_ui.select_tab(ShellTab::Chat);
            state.status_message = Some(i18n::text(Text::MentionInChat).to_owned());
            return;
        }
        LspHit::Add => {
            state.lsp_ui.open_editor();
            state.status_message = Some(i18n::text(Text::NoLanguageServers).to_owned());
            return;
        }
        LspHit::Toggle | LspHit::Primary | LspHit::Stop => {}
    }

    let Some(server) = state
        .lsp_servers
        .get(state.lsp_ui.selected_server())
        .cloned()
    else {
        state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
        return;
    };
    let command = match hit {
        LspHit::Toggle if server.runtime_available => OrchestratorCommand::LspSetEnabled {
            server: server.name.clone(),
            enabled: !server.enabled,
            scope: current_scope(state),
        },
        LspHit::Primary
            if server.runtime_available
                && server.enabled
                && server.detected
                && server.state != LspConnectionState::Starting =>
        {
            OrchestratorCommand::LspConnect {
                server: server.name.clone(),
                scope: current_scope(state),
            }
        }
        LspHit::Stop
            if matches!(
                server.state,
                LspConnectionState::Starting | LspConnectionState::Connected
            ) =>
        {
            OrchestratorCommand::LspDisconnect {
                server: server.name.clone(),
                scope: current_scope(state),
            }
        }
        _ => return,
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "{}: {} · {}",
            i18n::text(Text::LanguageIntelligence),
            server.name,
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn handle_lsp_editor_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.lsp_ui.close(),
        KeyCode::Tab => state.lsp_ui.editor_mut().next(),
        KeyCode::BackTab => state.lsp_ui.editor_mut().previous(),
        KeyCode::Backspace => state.lsp_ui.editor_mut().backspace(),
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && state.lsp_ui.editor().focus() == ConnectionField::Args =>
        {
            state.lsp_ui.editor_mut().newline();
        }
        KeyCode::Enter => {
            let field = state.lsp_ui.editor().focus();
            if is_connection_text_field(field) {
                state.lsp_ui.editor_mut().next();
            } else {
                handle_lsp_editor_field(field, state, command_tx);
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.lsp_ui.editor_mut().push(character);
        }
        _ => {}
    }
}

fn handle_lsp_editor_field(
    field: ConnectionField,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.lsp_ui.editor_mut().select(field);
    match field {
        ConnectionField::Required | ConnectionField::AutoStart => {
            state.lsp_ui.editor_mut().toggle(field);
        }
        ConnectionField::Save if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) => {
            state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        }
        ConnectionField::Save => match state.lsp_ui.editor().lsp_server() {
            Ok(server) => {
                if state
                    .lsp_servers
                    .iter()
                    .any(|existing| existing.name == server.name)
                {
                    state
                        .lsp_ui
                        .editor_mut()
                        .set_error(i18n::text(Text::AlreadyRunning));
                    return;
                }
                if try_send(
                    command_tx,
                    OrchestratorCommand::LspAddServer {
                        server,
                        scope: current_scope(state),
                    },
                    state,
                ) {
                    state.lsp_ui.editor_mut().close();
                    state.status_message = Some(i18n::text(Text::SaveConnection).to_owned());
                }
            }
            Err(error) => state.lsp_ui.editor_mut().set_error(error.to_string()),
        },
        ConnectionField::Cancel => state.lsp_ui.editor_mut().close(),
        _ => {}
    }
}

fn sync_lsp_selection(state: &mut AppState) {
    state
        .lsp_ui
        .sync(state.lsp_servers.as_ref(), state.lsp_diagnostics.as_ref());
}

fn handle_mcp_editor_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.mcp_ui.close(),
        KeyCode::Tab => state.mcp_ui.editor_mut().next(),
        KeyCode::BackTab => state.mcp_ui.editor_mut().previous(),
        KeyCode::Backspace => state.mcp_ui.editor_mut().backspace(),
        KeyCode::Enter
            if key.modifiers.contains(KeyModifiers::SHIFT)
                && matches!(
                    state.mcp_ui.editor().focus(),
                    ConnectionField::Args | ConnectionField::Mapping
                ) =>
        {
            state.mcp_ui.editor_mut().newline();
        }
        KeyCode::Enter => {
            let field = state.mcp_ui.editor().focus();
            if is_connection_text_field(field) {
                state.mcp_ui.editor_mut().next();
            } else {
                handle_mcp_editor_field(field, state, command_tx);
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.mcp_ui.editor_mut().push(character);
        }
        _ => {}
    }
}

fn handle_mcp_editor_field(
    field: ConnectionField,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    state.mcp_ui.editor_mut().select(field);
    match field {
        ConnectionField::Transport
        | ConnectionField::Required
        | ConnectionField::OAuth
        | ConnectionField::Approval
        | ConnectionField::Advanced => {
            state.mcp_ui.editor_mut().toggle(field);
        }
        ConnectionField::Save if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) => {
            state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        }
        ConnectionField::Save => match state.mcp_ui.editor().mcp_server() {
            Ok(server) => {
                if state
                    .mcp_servers
                    .iter()
                    .any(|existing| existing.name.eq_ignore_ascii_case(&server.name))
                {
                    state
                        .mcp_ui
                        .editor_mut()
                        .set_error(i18n::text(Text::AlreadyRunning));
                    return;
                }
                if try_send(
                    command_tx,
                    OrchestratorCommand::McpAddServer {
                        server,
                        scope: current_scope(state),
                    },
                    state,
                ) {
                    state.mcp_ui.editor_mut().close();
                    state.status_message = Some(i18n::text(Text::SaveConnection).to_owned());
                }
            }
            Err(error) => state.mcp_ui.editor_mut().set_error(error.to_string()),
        },
        ConnectionField::Cancel => state.mcp_ui.editor_mut().close(),
        _ => {}
    }
}

const fn is_connection_text_field(field: ConnectionField) -> bool {
    matches!(
        field,
        ConnectionField::Name
            | ConnectionField::Target
            | ConnectionField::Args
            | ConnectionField::CredentialEnv
            | ConnectionField::Mapping
            | ConnectionField::WorkingDirectory
            | ConnectionField::OAuthClientId
            | ConnectionField::OAuthScopes
            | ConnectionField::OAuthCallbackPort
            | ConnectionField::EnabledTools
            | ConnectionField::DisabledTools
            | ConnectionField::TrustedTools
            | ConnectionField::Language
            | ConnectionField::Extensions
            | ConnectionField::RootMarkers
    )
}

fn handle_mcp_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if state.mcp_ui.is_editing() {
        handle_mcp_editor_key(key, state, command_tx);
        return;
    }
    match key.code {
        KeyCode::Esc => handle_mcp_hit(McpHit::Close, state, command_tx),
        KeyCode::Tab => state.mcp_ui.next_focus(),
        KeyCode::BackTab => state.mcp_ui.previous_focus(),
        KeyCode::Left => state.mcp_ui.focus(McpFocus::Close),
        KeyCode::Right => state.mcp_ui.focus(McpFocus::Toggle),
        KeyCode::Up => state.mcp_ui.previous(),
        KeyCode::Down => state.mcp_ui.next(),
        KeyCode::Home => state.mcp_ui.first(),
        KeyCode::End => state.mcp_ui.last(),
        KeyCode::Enter => match state.mcp_ui.focused() {
            Some(McpFocus::Close) => handle_mcp_hit(McpHit::Close, state, command_tx),
            Some(McpFocus::Toggle) => handle_mcp_hit(McpHit::Toggle, state, command_tx),
            Some(McpFocus::Primary) => handle_mcp_hit(McpHit::Primary, state, command_tx),
            Some(McpFocus::Secondary) => handle_mcp_hit(McpHit::Secondary, state, command_tx),
            Some(McpFocus::Subagents) => handle_mcp_hit(McpHit::Subagents, state, command_tx),
            Some(McpFocus::Add) => handle_mcp_hit(McpHit::Add, state, command_tx),
            None => {}
        },
        _ => {}
    }
}

fn handle_mcp_hit(
    hit: McpHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if let McpHit::Item(index) = hit {
        state.mcp_ui.select(index);
        state.mcp_ui.focus(McpFocus::Primary);
        return;
    }
    if hit == McpHit::Close {
        state.mcp_ui.close();
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::McpConnections),
            i18n::text(Text::ClosedStatus)
        ));
        return;
    }
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    if hit == McpHit::Subagents {
        if !state.subagents.enabled {
            return;
        }
        state.mcp_ui.focus(McpFocus::Subagents);
        let enabled = !state.subagents.mcp_enabled;
        if try_send(
            command_tx,
            OrchestratorCommand::SetSubagentMcpAccess {
                enabled,
                scope: current_scope(state),
            },
            state,
        ) {
            state.status_message = Some(if enabled {
                i18n::text(Text::AllowSubagentsMcp).to_owned()
            } else {
                i18n::text(Text::DisabledForRun).to_owned()
            });
        }
        return;
    }
    if hit == McpHit::Add {
        state.mcp_ui.open_editor();
        state.status_message = Some(i18n::text(Text::NoMcpServersAdd).to_owned());
        return;
    }
    let Some(server) = state
        .mcp_servers
        .get(state.mcp_ui.selected_index())
        .cloned()
    else {
        state.status_message = Some(i18n::text(Text::NoServer).to_owned());
        return;
    };
    if hit == McpHit::Primary
        && (!server.runtime_available
            || !server.enabled
            || matches!(
                server.state,
                McpConnectionState::Connecting | McpConnectionState::Reconnecting
            ))
    {
        return;
    }
    if hit == McpHit::Secondary && (!server.runtime_available || !server.enabled || !server.oauth) {
        return;
    }
    let command = match hit {
        McpHit::Toggle if server.runtime_available => OrchestratorCommand::McpSetEnabled {
            server: server.name.clone(),
            enabled: !server.enabled,
            scope: current_scope(state),
        },
        McpHit::Primary if server.oauth && server.state == McpConnectionState::ReauthRequired => {
            OrchestratorCommand::McpBeginOAuth {
                server: server.name.clone(),
                scope: current_scope(state),
            }
        }
        McpHit::Primary if server.state == McpConnectionState::Connected => {
            OrchestratorCommand::McpDisconnect {
                server: server.name.clone(),
                scope: current_scope(state),
            }
        }
        McpHit::Primary => OrchestratorCommand::McpConnect {
            server: server.name.clone(),
            scope: current_scope(state),
        },
        McpHit::Secondary if server.oauth => OrchestratorCommand::McpForgetOAuth {
            server: server.name.clone(),
            scope: current_scope(state),
        },
        McpHit::Close
        | McpHit::Item(_)
        | McpHit::Toggle
        | McpHit::Secondary
        | McpHit::Subagents
        | McpHit::Add => return,
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(format!(
            "MCP: {} · {}",
            server.name,
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn handle_runtime_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.runtime_ui.close();
            state.status_message = Some(i18n::text(Text::RuntimeChangeCancelled).to_owned());
        }
        KeyCode::Tab => state.runtime_ui.next_focus(),
        KeyCode::BackTab => state.runtime_ui.previous_focus(),
        KeyCode::Left => state.runtime_ui.focus(RuntimeFocus::Back),
        KeyCode::Right => state.runtime_ui.focus(RuntimeFocus::Primary),
        KeyCode::Up => state.runtime_ui.previous(),
        KeyCode::Down => state.runtime_ui.next(),
        KeyCode::Home => state.runtime_ui.first(),
        KeyCode::End => state.runtime_ui.last(),
        KeyCode::Enter => match state.runtime_ui.focused() {
            Some(RuntimeFocus::Back) => {
                handle_runtime_hit(RuntimeHit::Back, state, command_tx);
            }
            Some(RuntimeFocus::Primary) => {
                handle_runtime_hit(RuntimeHit::Primary, state, command_tx);
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_runtime_hit(
    hit: RuntimeHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if hit == RuntimeHit::Primary
        && !matches!(
            state.phase,
            crate::agent::phase::AgentPhase::Idle
                | crate::agent::phase::AgentPhase::Error {
                    recoverable: true,
                    ..
                }
        )
    {
        state.status_message = Some(i18n::text(Text::RuntimeChangeIdleOnly).to_owned());
        return;
    }
    match hit {
        RuntimeHit::Item(index) => {
            state.runtime_ui.select(index);
            state.runtime_ui.focus(RuntimeFocus::Primary);
        }
        RuntimeHit::Back => {
            state.runtime_ui.back();
            if !state.runtime_ui.is_open() {
                state.status_message = Some(i18n::text(Text::RuntimeChangeCancelled).to_owned());
            }
        }
        RuntimeHit::Primary => match state.runtime_ui.stage() {
            RuntimeStage::Model | RuntimeStage::Effort | RuntimeStage::Context => {
                state.runtime_ui.advance();
            }
            RuntimeStage::Confirm => apply_runtime_settings(state, command_tx),
            RuntimeStage::Closed => {}
        },
    }
}

fn apply_runtime_settings(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let Some(deployment) = state
        .runtime_ui
        .selected_model()
        .map(str::to_owned)
        .filter(|model| {
            state
                .deployment_choices
                .iter()
                .any(|candidate| candidate == model)
        })
    else {
        state.status_message = Some(i18n::text(Text::SelectedDeploymentUnavailable).to_owned());
        state.runtime_ui.close();
        return;
    };
    let reasoning_effort = state.runtime_ui.selected_effort();
    let deep_thinking = state.runtime_ui.selected_ultra_profile();
    let context_budget = state.runtime_ui.selected_context_budget();
    let command = OrchestratorCommand::UpdateRuntimeSettings {
        deployment: deployment.clone(),
        reasoning_effort,
        deep_thinking,
        context_budget,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.runtime_ui.close();
        let updated = format!(
            "{}: {deployment} / {} / {}K {}",
            i18n::text(Text::RuntimeUpdated),
            if deep_thinking {
                "ultra (max + deep/pro)".to_owned()
            } else {
                reasoning_effort.to_string()
            },
            context_budget / 1_000,
            i18n::text(Text::ContextSuffix)
        );
        state.status_message =
            crate::onboarding::persist_default_context_budget(state.language, true, context_budget)
                .map_or_else(
                    |error| Some(format!("{}: {error}", i18n::text(Text::Failed))),
                    |_| Some(updated),
                );
    }
}

fn handle_palette_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    match key.code {
        KeyCode::Esc => {
            state.palette_ui.close();
            state.status_message = Some(i18n::text(Text::PaletteClosed).to_owned());
        }
        KeyCode::Tab => state.palette_ui.next_focus(),
        KeyCode::BackTab => state.palette_ui.previous_focus(),
        KeyCode::Left => state.palette_ui.focus(PaletteFocus::Close),
        KeyCode::Right => state.palette_ui.focus(PaletteFocus::Primary),
        KeyCode::Up => state.palette_ui.previous_item(),
        KeyCode::Down => state.palette_ui.next_item(),
        KeyCode::Home => state.palette_ui.first_item(),
        KeyCode::End => state.palette_ui.last_item(),
        KeyCode::Backspace => {
            if state.palette_ui.mode() == PaletteMode::Files && state.palette_ui.query().is_empty()
            {
                if let Err(error) = state.palette_ui.navigate_files_up() {
                    state.status_message =
                        Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
                }
            } else {
                state.palette_ui.pop_query();
            }
            update_palette_total(state);
        }
        KeyCode::Char(' ') if state.palette_ui.mode() == PaletteMode::Files => {
            state.palette_ui.toggle_current_file();
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.palette_ui.push_query(character);
            update_palette_total(state);
        }
        KeyCode::Enter => match state.palette_ui.focused() {
            Some(PaletteFocus::Close) => {
                handle_palette_hit(PaletteHit::Close, state, command_tx, urgent_control);
            }
            Some(PaletteFocus::Primary) => {
                handle_palette_hit(PaletteHit::Primary, state, command_tx, urgent_control);
            }
            None => {}
        },
        _ => {}
    }
}

fn update_palette_total(state: &mut AppState) {
    let query = state.palette_ui.query().to_lowercase();
    let total = match state.palette_ui.mode() {
        PaletteMode::Commands => command_matches(state.automation.commands.as_ref(), &query).len(),
        PaletteMode::Files => state.palette_ui.visible_file_count(),
        PaletteMode::Closed => 0,
    };
    state.palette_ui.set_total(total);
}

fn handle_palette_hit(
    hit: PaletteHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    match hit {
        PaletteHit::Item(index) => {
            state.palette_ui.select(index);
            state.palette_ui.focus(PaletteFocus::Primary);
        }
        PaletteHit::Close => {
            state.palette_ui.close();
            state.status_message = Some(i18n::text(Text::PaletteClosed).to_owned());
        }
        PaletteHit::Primary => apply_palette_selection(state, command_tx, urgent_control),
    }
}

fn apply_palette_selection(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    let query = state.palette_ui.query().to_lowercase();
    let selected = state.palette_ui.selected_index();
    match state.palette_ui.mode() {
        PaletteMode::Commands => {
            let selection = command_matches(state.automation.commands.as_ref(), &query)
                .get(selected)
                .map(|entry| entry.selection.clone());
            state.palette_ui.close();
            match selection {
                Some(PaletteCommandSelection::BuiltIn(action)) => {
                    handle_menu_action(action, state, command_tx, urgent_control);
                }
                Some(PaletteCommandSelection::Custom(id)) => {
                    let argument_hint = state
                        .automation
                        .commands
                        .iter()
                        .find(|command| command.id == id)
                        .map(|command| command.argument_hint.clone())
                        .unwrap_or_default();
                    insert_text(state, &format!("/{id} "));
                    state.status_message = Some(if argument_hint.is_empty() {
                        format!("/{id} · {}", i18n::text(Text::Ready))
                    } else {
                        format!(
                            "/{id} · {} · {}: {argument_hint}",
                            i18n::text(Text::Ready),
                            i18n::text(Text::ArgumentsPerLine)
                        )
                    });
                }
                None => {}
            }
        }
        PaletteMode::Files => {
            let paths = state.palette_ui.selected_or_current_files();
            if paths.is_empty() {
                activate_file_browser_entry(state);
            } else {
                attach_selected_paths(paths, state);
            }
        }
        PaletteMode::Closed => {}
    }
}

fn activate_file_browser_entry(state: &mut AppState) {
    match state.palette_ui.activate_current_file_entry() {
        Ok(FilePaletteAction::Navigated) => update_palette_total(state),
        Ok(FilePaletteAction::Attach(paths)) => attach_selected_paths(paths, state),
        Ok(FilePaletteAction::None) => {}
        Err(error) => {
            state.status_message = Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
        }
    }
}

fn attach_selected_paths(paths: Vec<PathBuf>, state: &mut AppState) {
    if paths.is_empty() {
        return;
    }
    if state.pending_attachments.len().saturating_add(paths.len()) > MAX_ATTACHMENTS_PER_TURN {
        state.status_message = Some(format!(
            "{}: {MAX_ATTACHMENTS_PER_TURN}",
            i18n::text(Text::AttachmentHint)
        ));
        return;
    }
    let drafts = paths
        .into_iter()
        .map(AttachmentDraft::snapshot_user_selected_path)
        .collect::<Result<Vec<_>, _>>();
    let drafts = match drafts {
        Ok(drafts) => drafts,
        Err(error) => {
            state.status_message = Some(format!("{}: {error}", i18n::text(Text::AttachmentHint)));
            return;
        }
    };
    state.palette_ui.close();
    for draft in drafts {
        attach_draft(state, draft);
    }
}

fn handle_shell_hit(
    hit: ShellHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    match hit {
        ShellHit::Session(index) => {
            open_sessions(state, command_tx);
            state.session_ui.select(index);
        }
        ShellHit::File(index) => {
            if let Some(path) = state.workspace_files.get(index).cloned() {
                attach_workspace_file(state, path);
            }
        }
        ShellHit::RemoveAttachment(index) => {
            if index < state.pending_attachments.len() {
                let removed = state.pending_attachments.remove(index);
                state.status_message = Some(format!(
                    "{} {}",
                    i18n::text(Text::RemovedAttachment),
                    removed.filename
                ));
            }
        }
        ShellHit::Tool(action_id) => {
            state.selected_tool = Some(action_id);
            if !state.expanded_tools.remove(&action_id) {
                state.expanded_tools.insert(action_id);
            }
        }
        ShellHit::JumpLatest => jump_to_latest(state),
        ShellHit::RetryFailedTurn => send_failed_turn_decision(state, command_tx, true),
        ShellHit::AbortFailedTurn => send_failed_turn_decision(state, command_tx, false),
        ShellHit::PauseTurn | ShellHit::ResumePausedTurn => {
            pause_or_resume(state, command_tx, urgent_control);
        }
        ShellHit::AbortPausedTurn => abort_paused_turn(state, command_tx),
        ShellHit::MascotFeed => {
            if state.phase.is_busy() {
                return;
            }
            let reaction = state.mascot.feed(std::time::Instant::now());
            state.status_message = Some(i18n::text(reaction.status_key()).to_owned());
        }
        ShellHit::MascotWake => {
            state.mascot.wake(std::time::Instant::now());
            state.status_message = Some(i18n::text(Text::Wake).to_owned());
        }
        ShellHit::McpManager => open_mcp(state),
        ShellHit::RuntimeManager => open_runtime(state),
        ShellHit::ModesManager => open_modes(state),
        ShellHit::FollowUps => open_follow_ups(state, false),
        ShellHit::SideChat => open_side_chat(state),
        ShellHit::UsageManager => open_usage(state),
        ShellHit::ReviewManager => open_reviews(state),
        ShellHit::NotificationCenter => open_notifications(state),
        ShellHit::InstructionsManager => open_instructions(state),
        ShellHit::SkillsManager => open_skills(state),
        ShellHit::PluginsManager => open_plugins(state),
        ShellHit::PrivacyShield => open_privacy(state),
        ShellHit::ShellPermissions => open_permissions(state),
        ShellHit::AutoApprovalCenter => open_auto_approval(state),
    }
}

fn pause_or_resume(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if state.phase.is_busy() {
        let Some(turn_id) = state.active_turn_id else {
            state.status_message = Some(i18n::text(Text::NoActiveTurn).to_owned());
            return;
        };
        let Some(control) = urgent_control else {
            state.status_message = Some(i18n::text(Text::SafePauseUnavailable).to_owned());
            return;
        };
        control.pause(turn_id);
        state.status_message = Some(format!(
            "{} #{turn_id} · {}",
            i18n::text(Text::PausingTurn),
            i18n::text(Text::ContinueDurableBoundary)
        ));
        return;
    }
    let Some(turn_id) = state.paused_turn_id else {
        state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
        return;
    };
    if try_send(
        command_tx,
        OrchestratorCommand::RetryTurn { turn_id },
        state,
    ) {
        state.status_message = Some(format!("{} #{turn_id}", i18n::text(Text::Resume)));
    }
}

fn abort_paused_turn(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let Some(turn_id) = state.paused_turn_id else {
        state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
        return;
    };
    if try_send(
        command_tx,
        OrchestratorCommand::AbortTurn { turn_id },
        state,
    ) {
        state.eta.cancel(turn_id);
        state.status_message = Some(format!("{} #{turn_id}", i18n::text(Text::Abort)));
    }
}

fn start_new_session(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !session_navigation_available(state) {
        state.status_message = Some(i18n::text(Text::SessionsIdleOnly).to_owned());
        return;
    }
    let command = OrchestratorCommand::NewSession {
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.status_message = Some(i18n::text(Text::NewSession).to_owned());
    }
}

fn scroll_history(state: &mut AppState, delta: i16) {
    state.shell_ui.follow_output = false;
    if delta.is_negative() {
        state.scroll_offset = state.scroll_offset.saturating_sub(delta.unsigned_abs());
    } else {
        state.scroll_offset = state.scroll_offset.saturating_add(delta as u16);
    }
}

fn jump_to_latest(state: &mut AppState) {
    state.shell_ui.follow_output = true;
    state.scroll_offset = u16::MAX;
    state.status_message = Some(i18n::text(Text::FollowingLatestOutput).to_owned());
}

fn open_sessions(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !session_navigation_available(state) {
        state.status_message = Some(i18n::text(Text::SessionsIdleOnly).to_owned());
        return;
    }
    state.session_ui.open(state.sessions.len());
    state.session_ui.sync(&state.sessions);
    if refresh_sessions(state, command_tx) {
        state.status_message = Some(i18n::text(Text::ManagePersistentSession).to_owned());
    }
}

fn refresh_sessions(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) -> bool {
    let command = OrchestratorCommand::RefreshSessions {
        query: state.session_ui.query().to_owned(),
        include_archived: state.session_ui.include_archived(),
    };
    try_send(command_tx, command, state)
}

fn handle_session_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            if matches!(state.session_ui.stage(), SessionStage::Picker) {
                state.session_ui.close();
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::SessionManager),
                    i18n::text(Text::ClosedStatus)
                ));
            } else {
                state.session_ui.back();
            }
        }
        KeyCode::Tab => advance_session_focus(state, false),
        KeyCode::BackTab => advance_session_focus(state, true),
        KeyCode::Left => state.session_ui.focus(SessionFocus::Close),
        KeyCode::Right => state.session_ui.focus(SessionFocus::Primary),
        KeyCode::Up
            if matches!(
                state.session_ui.stage(),
                SessionStage::Picker | SessionStage::Actions
            ) =>
        {
            state.session_ui.previous_item()
        }
        KeyCode::Down
            if matches!(
                state.session_ui.stage(),
                SessionStage::Picker | SessionStage::Actions
            ) =>
        {
            state.session_ui.next_item()
        }
        KeyCode::Home
            if matches!(
                state.session_ui.stage(),
                SessionStage::Picker | SessionStage::Actions
            ) =>
        {
            state.session_ui.first_item()
        }
        KeyCode::End
            if matches!(
                state.session_ui.stage(),
                SessionStage::Picker | SessionStage::Actions
            ) =>
        {
            state.session_ui.last_item()
        }
        KeyCode::Backspace if matches!(state.session_ui.stage(), SessionStage::Picker) => {
            state.session_ui.pop_query();
            refresh_sessions(state, command_tx);
        }
        KeyCode::Backspace if matches!(state.session_ui.stage(), SessionStage::Rename) => {
            state.session_ui.pop_rename();
        }
        KeyCode::Char(' ')
            if matches!(state.session_ui.stage(), SessionStage::Picker)
                && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            handle_session_hit(SessionHit::ToggleArchived, state, command_tx);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && matches!(state.session_ui.stage(), SessionStage::Picker) =>
        {
            state.session_ui.push_query(character);
            refresh_sessions(state, command_tx);
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                && matches!(state.session_ui.stage(), SessionStage::Rename) =>
        {
            state.session_ui.push_rename(character);
        }
        KeyCode::Enter => match state.session_ui.focused() {
            Some(SessionFocus::Close) => handle_session_hit(SessionHit::Close, state, command_tx),
            Some(SessionFocus::New) => handle_session_hit(SessionHit::New, state, command_tx),
            Some(SessionFocus::Actions) => {
                handle_session_hit(SessionHit::Actions, state, command_tx)
            }
            Some(SessionFocus::Primary) => {
                handle_session_hit(SessionHit::Primary, state, command_tx)
            }
            None => {}
        },
        _ => {}
    }
}

fn advance_session_focus(state: &mut AppState, backwards: bool) {
    if backwards {
        state.session_ui.previous_focus();
    } else {
        state.session_ui.next_focus();
    }
    if !matches!(state.session_ui.stage(), SessionStage::Picker) {
        while matches!(
            state.session_ui.focused(),
            Some(SessionFocus::New | SessionFocus::Actions)
        ) {
            if backwards {
                state.session_ui.previous_focus();
            } else {
                state.session_ui.next_focus();
            }
        }
    }
}

fn handle_session_hit(
    hit: SessionHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if !session_navigation_available(state) && matches!(hit, SessionHit::New | SessionHit::Primary)
    {
        state.status_message = Some(i18n::text(Text::SessionsIdleOnly).to_owned());
        return;
    }
    match hit {
        SessionHit::Launcher => open_sessions(state, command_tx),
        SessionHit::Item(index) => {
            state.session_ui.select(index);
            state.session_ui.focus(SessionFocus::Primary);
        }
        SessionHit::ActionItem(index) => {
            if matches!(state.session_ui.stage(), SessionStage::Actions) {
                while state.session_ui.selected_action() < index {
                    state.session_ui.next_item();
                }
                while state.session_ui.selected_action() > index {
                    state.session_ui.previous_item();
                }
                state.session_ui.focus(SessionFocus::Primary);
            }
        }
        SessionHit::ToggleArchived => {
            if matches!(state.session_ui.stage(), SessionStage::Picker) {
                state.session_ui.toggle_archived();
                refresh_sessions(state, command_tx);
            }
        }
        SessionHit::Close => {
            if matches!(
                state.session_ui.stage(),
                SessionStage::Picker | SessionStage::Closed
            ) {
                state.session_ui.close();
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::SessionManager),
                    i18n::text(Text::ClosedStatus)
                ));
            } else {
                state.session_ui.back();
            }
        }
        SessionHit::New if matches!(state.session_ui.stage(), SessionStage::Picker) => {
            let command = OrchestratorCommand::NewSession {
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.session_ui.close();
                state.status_message = Some(i18n::text(Text::NewSession).to_owned());
            }
        }
        SessionHit::Actions if matches!(state.session_ui.stage(), SessionStage::Picker) => {
            state.session_ui.bind_selected(&state.sessions);
            state.session_ui.open_actions();
        }
        SessionHit::Primary => handle_session_primary(state, command_tx),
        SessionHit::New | SessionHit::Actions => {}
    }
}

fn session_navigation_available(state: &AppState) -> bool {
    matches!(
        state.phase,
        crate::agent::phase::AgentPhase::Idle | crate::agent::phase::AgentPhase::Error { .. }
    )
}

fn handle_session_primary(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    match state.session_ui.stage() {
        SessionStage::Picker => prepare_session_intent(SessionIntent::Resume, state, command_tx),
        SessionStage::Actions => match state.session_ui.selected_action() {
            0 => prepare_session_intent(SessionIntent::Fork, state, command_tx),
            1 => {
                if let Some(title) = selected_session(state).map(|session| session.title) {
                    state.session_ui.begin_rename(&title);
                }
            }
            2 => update_selected_session(state, command_tx, SessionUpdate::Pin),
            3 => update_selected_session(state, command_tx, SessionUpdate::Archive),
            _ => {}
        },
        SessionStage::Rename => rename_selected_session(state, command_tx),
        SessionStage::WorkspaceConfirm => {
            execute_session_intent(state.session_ui.pending_intent(), true, state, command_tx)
        }
        SessionStage::Closed => {}
    }
}

fn prepare_session_intent(
    intent: SessionIntent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if matches!(state.session_ui.stage(), SessionStage::Picker)
        && !state.session_ui.bind_selected(&state.sessions)
    {
        state.status_message = Some(i18n::text(Text::SelectedSessionUnavailable).to_owned());
        return;
    }
    let mismatched = selected_session(state)
        .is_some_and(|session| session.workspace_root != state.workspace_root);
    if mismatched {
        state.session_ui.begin_workspace_confirmation(intent);
    } else {
        execute_session_intent(intent, false, state, command_tx);
    }
}

fn execute_session_intent(
    intent: SessionIntent,
    allow_workspace_mismatch: bool,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some(session) = selected_session(state) else {
        state.status_message = Some(i18n::text(Text::SelectedSessionUnavailable).to_owned());
        state.session_ui.back();
        return;
    };
    let command = match intent {
        SessionIntent::Resume => OrchestratorCommand::ResumeSession {
            session_id: session.id,
            allow_workspace_mismatch,
            scope: current_scope(state),
        },
        SessionIntent::Fork => OrchestratorCommand::ForkSession {
            session_id: session.id,
            scope: current_scope(state),
        },
    };
    if try_send(command_tx, command, state) {
        state.session_ui.close();
        state.status_message = Some(match intent {
            SessionIntent::Resume => i18n::text(Text::Resume).to_owned(),
            SessionIntent::Fork => i18n::text(Text::ForkNewSession).to_owned(),
        });
    }
}

#[derive(Debug, Clone, Copy)]
enum SessionUpdate {
    Pin,
    Archive,
}

fn update_selected_session(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    update: SessionUpdate,
) {
    let Some(session) = selected_session(state) else {
        state.status_message = Some(i18n::text(Text::SelectedSessionUnavailable).to_owned());
        state.session_ui.back();
        return;
    };
    let command = match update {
        SessionUpdate::Pin => OrchestratorCommand::SetSessionPinned {
            session_id: session.id,
            pinned: !session.pinned,
            scope: current_scope(state),
        },
        SessionUpdate::Archive => OrchestratorCommand::SetSessionArchived {
            session_id: session.id,
            archived: !session.archived,
            scope: current_scope(state),
        },
    };
    if try_send(command_tx, command, state) {
        state.session_ui.back();
        state.status_message = Some(format!(
            "{}: {}",
            i18n::text(Text::Session),
            i18n::text(Text::UpdatingStatus)
        ));
    }
}

fn rename_selected_session(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let Some(session) = selected_session(state) else {
        state.status_message = Some(i18n::text(Text::SelectedSessionUnavailable).to_owned());
        state.session_ui.back();
        return;
    };
    let title = state.session_ui.rename_buffer().trim().to_owned();
    if title.len() > 512 || !crate::agent::side_chat::has_visible_text(&title) {
        state.status_message = Some(i18n::text(Text::VisibleTextRequired).to_owned());
        return;
    }
    let command = OrchestratorCommand::RenameSession {
        session_id: session.id,
        title,
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.session_ui.back();
        state.status_message = Some(i18n::text(Text::RenameSession).to_owned());
    }
}

fn selected_session(state: &AppState) -> Option<crate::agent::SessionSummary> {
    state.session_ui.selected_session_id().map_or_else(
        || {
            state
                .sessions
                .get(state.session_ui.selected_index())
                .cloned()
        },
        |id| {
            state
                .sessions
                .iter()
                .find(|session| &session.id == id)
                .cloned()
        },
    )
}

const fn current_scope(state: &AppState) -> CommandScope {
    CommandScope {
        conversation_epoch: state.conversation_epoch,
        phase_revision: state.phase_revision,
    }
}

fn open_rewind(state: &mut AppState) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    if state.checkpoints.is_empty() {
        state.status_message = Some(i18n::text(Text::NoCheckpoints).to_owned());
        return;
    }
    state.rewind_ui.open(state.checkpoints.len());
    state.status_message = Some(i18n::text(Text::ReviewCheckpoint).to_owned());
}

fn handle_rewind_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => {
            state.rewind_ui.close();
            state.status_message = Some(i18n::text(Text::Cancelled).to_owned());
        }
        KeyCode::Tab => state.rewind_ui.next_focus(),
        KeyCode::BackTab => state.rewind_ui.previous_focus(),
        KeyCode::Left => state.rewind_ui.focus(RewindFocus::Secondary),
        KeyCode::Right => state.rewind_ui.focus(RewindFocus::Primary),
        KeyCode::Up if matches!(state.rewind_ui.stage(), RewindStage::Picker) => {
            state.rewind_ui.previous_item();
        }
        KeyCode::Down if matches!(state.rewind_ui.stage(), RewindStage::Picker) => {
            state.rewind_ui.next_item();
        }
        KeyCode::Home if matches!(state.rewind_ui.stage(), RewindStage::Picker) => {
            state.rewind_ui.first_item();
        }
        KeyCode::End if matches!(state.rewind_ui.stage(), RewindStage::Picker) => {
            state.rewind_ui.last_item();
        }
        KeyCode::Enter => match state.rewind_ui.focused() {
            Some(RewindFocus::Secondary) => {
                handle_rewind_hit(RewindHit::Secondary, state, command_tx)
            }
            Some(RewindFocus::Primary) => {
                handle_rewind_hit(RewindHit::Primary, state, command_tx);
            }
            None => {}
        },
        _ => {}
    }
}

fn handle_rewind_hit(
    hit: RewindHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        RewindHit::Launcher => open_rewind(state),
        RewindHit::Item(index) => {
            state.rewind_ui.select(index);
            state.rewind_ui.focus(RewindFocus::Primary);
        }
        RewindHit::Secondary => match state.rewind_ui.stage() {
            RewindStage::Picker | RewindStage::Closed => {
                state.rewind_ui.close();
                state.status_message = Some(i18n::text(Text::Cancelled).to_owned());
            }
            RewindStage::Confirm => state.rewind_ui.back(),
        },
        RewindHit::Primary => match state.rewind_ui.stage() {
            RewindStage::Picker => state.rewind_ui.review(&state.checkpoints),
            RewindStage::Confirm => apply_rewind(state, command_tx),
            RewindStage::Closed => {}
        },
    }
}

fn apply_rewind(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !matches!(state.phase, crate::agent::phase::AgentPhase::Idle) {
        state.status_message = Some(i18n::text(Text::IdleChangesOnly).to_owned());
        return;
    }
    let Some(checkpoint_id) = state.rewind_ui.reviewed_checkpoint_id().filter(|id| {
        state
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.id == *id)
    }) else {
        state.rewind_ui.close();
        state.status_message = Some(i18n::text(Text::SelectedCheckpointUnavailable).to_owned());
        return;
    };
    let command = OrchestratorCommand::Rewind {
        checkpoint_id,
        scope: CommandScope {
            conversation_epoch: state.conversation_epoch,
            phase_revision: state.phase_revision,
        },
    };
    if try_send(command_tx, command, state) {
        state.rewind_ui.close();
        state.status_message = Some(format!(
            "{} #{checkpoint_id}",
            i18n::text(Text::CheckpointRewind)
        ));
    }
}

fn handle_confirmation_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if is_control_char(key, 'c') {
        interrupt_modal(state, command_tx, urgent_control);
        return;
    }
    if is_control_char(key, 'r') {
        reset_from_modal(state, command_tx, urgent_control);
        return;
    }

    match key.code {
        KeyCode::Tab => state.confirmation_ui.next(),
        KeyCode::BackTab => state.confirmation_ui.previous(),
        KeyCode::Left => state.confirmation_ui.focus(ConfirmationChoice::Decline),
        KeyCode::Right => state.confirmation_ui.focus(ConfirmationChoice::Approve),
        KeyCode::Up => {
            state.confirmation_end_requested = false;
            state.confirmation_scroll = state.confirmation_scroll.saturating_sub(1);
        }
        KeyCode::PageUp => {
            state.confirmation_end_requested = false;
            state.confirmation_scroll = state.confirmation_scroll.saturating_sub(10);
        }
        KeyCode::Down => scroll_confirmation(state, 1),
        KeyCode::PageDown => scroll_confirmation(state, 10),
        KeyCode::End => {
            if state.confirmation_view_ready {
                state.confirmation_scroll = state.confirmation_max_scroll;
                state.confirmation_end_requested = true;
            } else {
                state.status_message = Some(i18n::text(Text::ConfirmationResize).to_owned());
            }
        }
        KeyCode::Home => {
            state.confirmation_end_requested = false;
            state.confirmation_scroll = 0;
        }
        KeyCode::Enter => {
            if let Some(choice) = state.confirmation_ui.selected() {
                decide_confirmation(choice, state, command_tx);
            }
        }
        KeyCode::Char('y' | 'Y') => {
            decide_confirmation(ConfirmationChoice::Approve, state, command_tx);
        }
        KeyCode::Char('t' | 'T') if state.pending_confirmation.is_some() => {
            decide_confirmation(ConfirmationChoice::TrustExactForSession, state, command_tx);
        }
        KeyCode::Char('n' | 'N') | KeyCode::Esc => {
            decide_confirmation(ConfirmationChoice::Decline, state, command_tx);
        }
        _ => {}
    }
}

fn scroll_confirmation(state: &mut AppState, rows: usize) {
    state.confirmation_end_requested = false;
    state.confirmation_scroll = state
        .confirmation_scroll
        .saturating_add(rows)
        .min(state.confirmation_max_scroll);
}

fn decide_confirmation(
    choice: ConfirmationChoice,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some((turn_id, action_id)) = state.pending_confirmation_ids() else {
        return;
    };
    let approved = choice != ConfirmationChoice::Decline;
    if approved && !state.confirmation_view_ready {
        state.status_message = Some(i18n::text(Text::ConfirmationResize).to_owned());
        return;
    }
    if approved && state.confirmation_max_scroll > 0 && !state.confirmation_suffix_viewed {
        state.confirmation_scroll = state.confirmation_max_scroll;
        state.confirmation_end_requested = true;
        state.status_message = Some(i18n::text(Text::ReviewToEnd).to_owned());
        return;
    }
    if choice == ConfirmationChoice::TrustExactForSession
        && state
            .pending_confirmation
            .as_ref()
            .is_none_or(|pending| !pending.session_trust_available)
    {
        state.status_message = Some(i18n::text(Text::ForcedConfirmationHelp).to_owned());
        return;
    }
    let decision = match choice {
        ConfirmationChoice::Decline => ShellApprovalDecision::Decline,
        ConfirmationChoice::Approve => ShellApprovalDecision::RunOnce,
        ConfirmationChoice::TrustExactForSession => ShellApprovalDecision::TrustExactForSession,
    };
    let command = OrchestratorCommand::Confirm {
        turn_id,
        action_id,
        decision,
    };
    if try_send(command_tx, command, state) {
        state.pending_confirmation = None;
        state.pending_mcp_confirmation = None;
        state.confirmation_ui.reset();
        state.confirmation_scroll = 0;
        state.confirmation_max_scroll = 0;
        state.confirmation_view_ready = false;
        state.confirmation_suffix_viewed = false;
        state.confirmation_end_requested = false;
        state.status_message = Some(match choice {
            ConfirmationChoice::Decline => i18n::text(Text::Declined).to_owned(),
            ConfirmationChoice::Approve => i18n::text(Text::ApproveExecute).to_owned(),
            ConfirmationChoice::TrustExactForSession => {
                i18n::text(Text::SessionGrantHelp).to_owned()
            }
        });
    }
}

fn handle_continuation_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if is_control_char(key, 'c') {
        interrupt_modal(state, command_tx, urgent_control);
        return;
    }
    if is_control_char(key, 'r') {
        reset_from_modal(state, command_tx, urgent_control);
        return;
    }
    match key.code {
        KeyCode::Tab => state.continuation_ui.next(),
        KeyCode::BackTab => state.continuation_ui.previous(),
        KeyCode::Left => state.continuation_ui.focus(ContinuationChoice::Stop),
        KeyCode::Right => state.continuation_ui.focus(ContinuationChoice::Continue),
        KeyCode::Enter => {
            if let Some(choice) = state.continuation_ui.selected() {
                decide_continuation(
                    matches!(choice, ContinuationChoice::Continue),
                    state,
                    command_tx,
                );
            }
        }
        KeyCode::Char('y' | 'Y') => decide_continuation(true, state, command_tx),
        KeyCode::Char('n' | 'N') | KeyCode::Esc => {
            decide_continuation(false, state, command_tx);
        }
        _ => {}
    }
}

fn handle_agents_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Tab => state.agents_ui.next_focus(),
        KeyCode::BackTab => state.agents_ui.previous_focus(),
        KeyCode::Up => state.agents_ui.previous_item(&state.subagents),
        KeyCode::Down => state.agents_ui.next_item(&state.subagents),
        KeyCode::Home => state.agents_ui.first_item(&state.subagents),
        KeyCode::End => state.agents_ui.last_item(&state.subagents),
        KeyCode::Esc => state.shell_ui.select_tab(ShellTab::Chat),
        KeyCode::Enter => {
            if let Some(focus) = state.agents_ui.browse_focused() {
                handle_agent_browse_action(focus, state, command_tx);
            }
        }
        _ => {}
    }
}

fn handle_agent_editor_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Esc => state.agents_ui.close_editor(),
        KeyCode::Tab => state.agents_ui.next_focus(),
        KeyCode::BackTab => state.agents_ui.previous_focus(),
        KeyCode::Up if state.agents_ui.editor() == AgentEditor::Spawn => {
            match state.agents_ui.dialog_focused() {
                Some(AgentDialogFocus::Profiles) => {
                    state.agents_ui.previous_profile(&state.subagents);
                }
                Some(AgentDialogFocus::Dependencies) => {
                    state.agents_ui.previous_dependency(&state.subagents);
                }
                Some(AgentDialogFocus::Claims) => {
                    state
                        .agents_ui
                        .previous_claim(state.workspace_files.as_ref());
                }
                _ => {}
            }
        }
        KeyCode::Down if state.agents_ui.editor() == AgentEditor::Spawn => {
            match state.agents_ui.dialog_focused() {
                Some(AgentDialogFocus::Profiles) => {
                    state.agents_ui.next_profile(&state.subagents);
                }
                Some(AgentDialogFocus::Dependencies) => {
                    state.agents_ui.next_dependency(&state.subagents);
                }
                Some(AgentDialogFocus::Claims) => {
                    state.agents_ui.next_claim(state.workspace_files.as_ref());
                }
                _ => {}
            }
        }
        KeyCode::Left => state.agents_ui.focus_dialog(AgentDialogFocus::Cancel),
        KeyCode::Right => state.agents_ui.focus_dialog(AgentDialogFocus::Submit),
        KeyCode::Backspace => state.agents_ui.pop(),
        KeyCode::Enter => match state.agents_ui.dialog_focused() {
            Some(AgentDialogFocus::Profiles) => {
                state.agents_ui.focus_dialog(AgentDialogFocus::Dependencies);
            }
            Some(AgentDialogFocus::Dependencies) => {
                state.agents_ui.toggle_selected_dependency(&state.subagents);
            }
            Some(AgentDialogFocus::Claims) => {
                state
                    .agents_ui
                    .toggle_selected_claim(&state.subagents, state.workspace_files.as_ref());
            }
            Some(AgentDialogFocus::Task) => submit_agent_editor(state, command_tx),
            Some(AgentDialogFocus::Cancel) => state.agents_ui.close_editor(),
            Some(AgentDialogFocus::Submit) => submit_agent_editor(state, command_tx),
            None => {}
        },
        KeyCode::Char(' ')
            if state.agents_ui.editor() == AgentEditor::Spawn
                && matches!(
                    state.agents_ui.dialog_focused(),
                    Some(AgentDialogFocus::Dependencies | AgentDialogFocus::Claims)
                ) =>
        {
            match state.agents_ui.dialog_focused() {
                Some(AgentDialogFocus::Dependencies) => {
                    state.agents_ui.toggle_selected_dependency(&state.subagents)
                }
                Some(AgentDialogFocus::Claims) => state
                    .agents_ui
                    .toggle_selected_claim(&state.subagents, state.workspace_files.as_ref()),
                _ => {}
            }
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.agents_ui.push(character);
        }
        _ => {}
    }
}

fn handle_agent_hit(
    hit: AgentHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        AgentHit::Item(index) => state.agents_ui.select_index(&state.subagents, index),
        AgentHit::Profile(index) => state.agents_ui.select_profile(&state.subagents, index),
        AgentHit::Dependency(index) => {
            state.agents_ui.toggle_dependency(&state.subagents, index);
        }
        AgentHit::Claim(index) => {
            state
                .agents_ui
                .toggle_claim(&state.subagents, state.workspace_files.as_ref(), index);
        }
        AgentHit::Browse(focus) => {
            state.agents_ui.focus_browse(focus);
            handle_agent_browse_action(focus, state, command_tx);
        }
        AgentHit::Dialog(AgentDialogFocus::Cancel) => state.agents_ui.close_editor(),
        AgentHit::Dialog(AgentDialogFocus::Submit) => submit_agent_editor(state, command_tx),
        AgentHit::Dialog(
            focus @ (AgentDialogFocus::Profiles
            | AgentDialogFocus::Dependencies
            | AgentDialogFocus::Claims
            | AgentDialogFocus::Task),
        ) => {
            state.agents_ui.focus_dialog(focus);
        }
        AgentHit::Command(focus) => decide_subagent_command(
            matches!(focus, AgentDecisionFocus::Approve),
            state,
            command_tx,
        ),
        AgentHit::Binary(focus) => decide_subagent_binary(
            matches!(focus, AgentDecisionFocus::Approve),
            state,
            command_tx,
        ),
    }
}

fn handle_agent_browse_action(
    focus: AgentBrowseFocus,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match focus {
        AgentBrowseFocus::List => {}
        AgentBrowseFocus::New => state.agents_ui.open_spawn(&state.subagents),
        AgentBrowseFocus::Reload => {
            if try_send(
                command_tx,
                OrchestratorCommand::ReloadSubagentProfiles,
                state,
            ) {
                state.status_message = Some(i18n::text(Text::ReloadProfiles).to_owned());
            }
        }
        AgentBrowseFocus::Message => {
            if state
                .agents_ui
                .selected(&state.subagents)
                .is_some_and(|agent| agent.status.is_active())
            {
                state.agents_ui.open_message();
            }
        }
        AgentBrowseFocus::Stop => {
            let Some(agent) = state.agents_ui.selected(&state.subagents).cloned() else {
                return;
            };
            if !agent.status.is_active() {
                return;
            }
            let command = OrchestratorCommand::CancelSubagent {
                agent_id: agent.id,
                expected_revision: agent.revision,
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::StoppingStatus),
                    agent.id
                ));
            }
        }
        AgentBrowseFocus::RaiseBudget | AgentBrowseFocus::StopAtBudget => {
            let Some(agent) = state.agents_ui.selected(&state.subagents).cloned() else {
                return;
            };
            if agent.pending_budget.is_none() {
                return;
            }
            let approved = focus == AgentBrowseFocus::RaiseBudget;
            let command = OrchestratorCommand::DecideSubagentBudget {
                agent_id: agent.id,
                expected_revision: agent.revision,
                approved,
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(if approved {
                    format!("{}: {}", i18n::text(Text::RaiseBudget50K), agent.id)
                } else {
                    format!("{}: {}", i18n::text(Text::StopBranch), agent.id)
                });
            }
        }
        AgentBrowseFocus::Resume => {
            let Some(agent) = state.agents_ui.selected(&state.subagents).cloned() else {
                return;
            };
            let can_resume = agent.status.is_recoverable()
                && agent
                    .recovery
                    .as_ref()
                    .is_some_and(|recovery| recovery.can_resume);
            if !can_resume {
                return;
            }
            let command = OrchestratorCommand::ResumeSubagent {
                agent_id: agent.id,
                expected_revision: agent.revision,
            };
            if try_send(command_tx, command, state) {
                state.status_message =
                    Some(format!("{}: {}", i18n::text(Text::ResumeSafely), agent.id));
            }
        }
        AgentBrowseFocus::Abandon => {
            let Some(agent) = state.agents_ui.selected(&state.subagents).cloned() else {
                return;
            };
            if !agent.status.is_recoverable() {
                return;
            }
            let command = OrchestratorCommand::AbandonSubagentRecovery {
                agent_id: agent.id,
                expected_revision: agent.revision,
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(format!(
                    "{}: {}",
                    i18n::text(Text::AbandonRecovery),
                    agent.id
                ));
            }
        }
        AgentBrowseFocus::Review => {
            if !matches!(state.phase, crate::agent::AgentPhase::Idle) {
                return;
            }
            let Some(agent) = state.agents_ui.selected(&state.subagents).cloned() else {
                return;
            };
            let (Some(path), Some(change_digest)) =
                (agent.changed_files.first(), agent.change_digest.as_ref())
            else {
                return;
            };
            let command = OrchestratorCommand::OpenSubagentReview {
                agent_id: agent.id,
                expected_revision: agent.revision,
                change_digest: change_digest.clone(),
                path: path.clone(),
                scope: current_scope(state),
            };
            if try_send(command_tx, command, state) {
                state.status_message = Some(format!("{}: {path}", i18n::text(Text::ReviewChanges)));
            }
        }
    }
}

fn submit_agent_editor(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !state.agents_ui.buffer_has_visible_text() {
        state.status_message = Some(i18n::text(Text::VisibleTextRequired).to_owned());
        return;
    }
    let value = state.agents_ui.buffer().trim().to_owned();
    let command = match state.agents_ui.editor() {
        AgentEditor::Spawn => {
            let Some(profile_id) = state.agents_ui.selected_profile_id().map(str::to_owned) else {
                state.status_message = Some(i18n::text(Text::NoProfileAvailable).to_owned());
                return;
            };
            OrchestratorCommand::SpawnSubagent {
                task: value,
                profile_id,
                dependencies: state.agents_ui.selected_dependencies(),
                file_claims: state.agents_ui.selected_file_claims(),
            }
        }
        AgentEditor::Message => {
            let Some(agent) = state.agents_ui.selected(&state.subagents) else {
                state.status_message =
                    Some(i18n::text(Text::SelectedSubagentUnavailable).to_owned());
                return;
            };
            OrchestratorCommand::MessageSubagent {
                agent_id: agent.id,
                expected_revision: agent.revision,
                message: value,
            }
        }
        AgentEditor::Closed => return,
    };
    if try_send(command_tx, command, state) {
        state.agents_ui.close_editor();
        state.status_message = Some(i18n::text(Text::FixQueuedLabel).to_owned());
    }
}

fn handle_subagent_command_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match key.code {
        KeyCode::Tab => state.agents_ui.next_command_focus(),
        KeyCode::BackTab => state.agents_ui.previous_command_focus(),
        KeyCode::Left => state.agents_ui.focus_command(AgentDecisionFocus::Decline),
        KeyCode::Right => state.agents_ui.focus_command(AgentDecisionFocus::Approve),
        KeyCode::Esc | KeyCode::Char('n' | 'N') => {
            decide_subagent_command(false, state, command_tx);
        }
        KeyCode::Char('y' | 'Y') => decide_subagent_command(true, state, command_tx),
        KeyCode::Enter => {
            if let Some(focus) = state.agents_ui.command_focused() {
                decide_subagent_command(
                    matches!(focus, AgentDecisionFocus::Approve),
                    state,
                    command_tx,
                );
            }
        }
        _ => {}
    }
}

fn handle_subagent_command_hit(
    hit: AgentHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if let AgentHit::Command(focus) = hit {
        state.agents_ui.focus_command(focus);
        decide_subagent_command(
            matches!(focus, AgentDecisionFocus::Approve),
            state,
            command_tx,
        );
    }
}

fn decide_subagent_command(
    approved: bool,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some((index, agent, pending)) =
        state
            .subagents
            .agents
            .iter()
            .enumerate()
            .find_map(|(index, agent)| {
                agent
                    .pending_command
                    .clone()
                    .map(|pending| (index, agent.clone(), pending))
            })
    else {
        return;
    };
    let command = OrchestratorCommand::DecideSubagentCommand {
        agent_id: agent.id,
        expected_revision: agent.revision,
        action_id: pending.action_id,
        approved,
    };
    if try_send(command_tx, command, state) {
        if let Some(local) = Arc::make_mut(&mut state.subagents.agents).get_mut(index) {
            local.pending_command = None;
        }
        state.agents_ui.hide_command_dialog();
        state.status_message = Some(if approved {
            format!(
                "{}: {} · {}",
                i18n::text(Text::ApproveExecute),
                if pending.mcp {
                    i18n::text(Text::McpToolNoun)
                } else {
                    i18n::text(Text::ShellCommandNoun)
                },
                agent.id
            )
        } else {
            format!(
                "{}: {} · {}",
                i18n::text(Text::Declined),
                if pending.mcp {
                    i18n::text(Text::McpToolNoun)
                } else {
                    i18n::text(Text::ShellCommandNoun)
                },
                agent.id
            )
        });
    }
}

fn handle_subagent_review_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if is_control_char(key, 'c') || is_control_char(key, 'r') {
        if state
            .pending_subagent_review
            .as_ref()
            .is_some_and(|pending| pending.review.binary)
        {
            decide_subagent_binary(false, state, command_tx);
        } else {
            state.patch_review_ui.decide_all(false);
            submit_patch_decisions(state, command_tx);
        }
        return;
    }
    let is_binary = state
        .pending_subagent_review
        .as_ref()
        .is_some_and(|pending| pending.review.binary);
    if !is_binary {
        handle_patch_review_key(key, state, command_tx, None);
        return;
    }
    match key.code {
        KeyCode::Tab => state.agents_ui.next_binary_focus(),
        KeyCode::BackTab => state.agents_ui.previous_binary_focus(),
        KeyCode::Left => state.agents_ui.focus_binary(AgentDecisionFocus::Decline),
        KeyCode::Right => state.agents_ui.focus_binary(AgentDecisionFocus::Approve),
        KeyCode::Esc | KeyCode::Char('n' | 'N') => {
            decide_subagent_binary(false, state, command_tx);
        }
        KeyCode::Char('y' | 'Y') => decide_subagent_binary(true, state, command_tx),
        KeyCode::Enter => {
            if let Some(focus) = state.agents_ui.binary_focused() {
                decide_subagent_binary(
                    matches!(focus, AgentDecisionFocus::Approve),
                    state,
                    command_tx,
                );
            }
        }
        _ => {}
    }
}

fn handle_subagent_review_mouse(
    mouse: MouseEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return;
    }
    let is_binary = state
        .pending_subagent_review
        .as_ref()
        .is_some_and(|pending| pending.review.binary);
    if is_binary {
        if let Some(AgentHit::Binary(focus)) = state.agents_ui.clicked(mouse.column, mouse.row) {
            state.agents_ui.focus_binary(focus);
            decide_subagent_binary(
                matches!(focus, AgentDecisionFocus::Approve),
                state,
                command_tx,
            );
        }
    } else if let Some(hit) = state.patch_review_ui.clicked(mouse.column, mouse.row) {
        handle_patch_review_hit(hit, state, command_tx);
    }
}

fn decide_subagent_binary(
    approved: bool,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some(pending) = state.pending_subagent_review.as_ref() else {
        return;
    };
    let command = OrchestratorCommand::DecideSubagentFile {
        review: Arc::clone(&pending.review),
        decision: if approved {
            SubagentFileDecision::ApproveBinary
        } else {
            SubagentFileDecision::Reject
        },
        scope: current_scope(state),
    };
    if try_send(command_tx, command, state) {
        state.pending_subagent_review = None;
        state.agents_ui.hide_binary_dialog();
        state.status_message = Some(if approved {
            i18n::text(Text::ApproveWholeFile).to_owned()
        } else {
            i18n::text(Text::RejectFile).to_owned()
        });
    }
}

fn handle_patch_review_key(
    key: KeyEvent,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if is_control_char(key, 'c') {
        interrupt_modal(state, command_tx, urgent_control);
        return;
    }
    if is_control_char(key, 'r') {
        reset_from_modal(state, command_tx, urgent_control);
        return;
    }
    match key.code {
        KeyCode::Tab => state.patch_review_ui.next_focus(),
        KeyCode::BackTab => state.patch_review_ui.previous_focus(),
        KeyCode::Up => state.patch_review_ui.previous_hunk(),
        KeyCode::Down => state.patch_review_ui.next_hunk(),
        KeyCode::Char('a' | 'A') => state.patch_review_ui.decide_current(true),
        KeyCode::Char('r' | 'R') => state.patch_review_ui.decide_current(false),
        KeyCode::Esc => {
            handle_patch_review_action(PatchReviewFocus::Cancel, state, command_tx);
        }
        KeyCode::Enter => {
            if let Some(focus) = state.patch_review_ui.focused() {
                handle_patch_review_action(focus, state, command_tx);
            }
        }
        _ => {}
    }
}

fn handle_patch_review_hit(
    hit: PatchReviewHit,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match hit {
        PatchReviewHit::Hunk(index) => state.patch_review_ui.select_hunk(index),
        PatchReviewHit::Action(action) => {
            state.patch_review_ui.focus(action);
            handle_patch_review_action(action, state, command_tx);
        }
    }
}

fn handle_patch_review_action(
    action: PatchReviewFocus,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    match action {
        PatchReviewFocus::RejectHunk => state.patch_review_ui.decide_current(false),
        PatchReviewFocus::AcceptHunk => state.patch_review_ui.decide_current(true),
        PatchReviewFocus::RejectAll => state.patch_review_ui.decide_all(false),
        PatchReviewFocus::Cancel => {
            state.patch_review_ui.decide_all(false);
            submit_patch_decisions(state, command_tx);
        }
        PatchReviewFocus::AcceptAll => state.patch_review_ui.decide_all(true),
        PatchReviewFocus::Apply => submit_patch_decisions(state, command_tx),
    }
}

fn submit_patch_decisions(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    let Some(decisions) = state.patch_review_ui.decisions() else {
        state.status_message = Some(format!(
            "{} ({}/{})",
            i18n::text(Text::PatchHunkHelp),
            state.patch_review_ui.completed(),
            state.pending_patch_review.as_ref().map_or_else(
                || {
                    state
                        .pending_subagent_review
                        .as_ref()
                        .and_then(|pending| pending.review.review.as_ref())
                        .map_or(0, |review| review.hunks.len())
                },
                |pending| pending.review.hunks.len(),
            )
        ));
        return;
    };
    if let Some(pending) = state.pending_subagent_review.as_ref() {
        let command = OrchestratorCommand::DecideSubagentFile {
            review: Arc::clone(&pending.review),
            decision: SubagentFileDecision::TextHunks(decisions.clone()),
            scope: current_scope(state),
        };
        if try_send(command_tx, command, state) {
            let approved = decisions.iter().filter(|decision| **decision).count();
            state.pending_subagent_review = None;
            state.patch_review_ui.close();
            state.status_message = Some(format!(
                "{}: {approved}/{}",
                i18n::text(Text::ReviewPatchHunks),
                decisions.len()
            ));
        }
        return;
    }
    let Some(pending) = state.pending_patch_review.as_ref() else {
        return;
    };
    let command = OrchestratorCommand::DecidePatch {
        turn_id: pending.turn_id,
        action_id: pending.action_id,
        decisions: decisions.clone(),
    };
    if try_send(command_tx, command, state) {
        let approved = decisions.iter().filter(|decision| **decision).count();
        state.pending_patch_review = None;
        state.patch_review_ui.close();
        state.status_message = Some(format!(
            "{}: {approved}/{}",
            i18n::text(Text::ReviewPatchHunks),
            decisions.len()
        ));
    }
}

fn decide_continuation(
    continue_loop: bool,
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
) {
    let Some(pending) = state.pending_continuation else {
        return;
    };
    let command = OrchestratorCommand::ContinueToolLoop {
        turn_id: pending.turn_id,
        continuation_id: pending.continuation_id,
        continue_loop,
    };
    if try_send(command_tx, command, state) {
        state.pending_continuation = None;
        state.continuation_ui.reset();
        state.status_message = Some(if continue_loop {
            i18n::text(Text::Continue).to_owned()
        } else {
            i18n::text(Text::StopLabel).to_owned()
        });
    }
}

fn interrupt_modal(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    let turn_id = state
        .pending_confirmation_ids()
        .map(|(turn_id, _)| turn_id)
        .or_else(|| {
            state
                .pending_patch_review
                .as_ref()
                .map(|pending| pending.turn_id)
        })
        .or_else(|| state.pending_continuation.map(|pending| pending.turn_id));
    let Some(turn_id) = turn_id else {
        return;
    };
    if let Some(control) = urgent_control {
        control.interrupt(turn_id);
        state.status_message = Some(format!("{} #{turn_id}", i18n::text(Text::InterruptingTurn)));
    } else if try_send(
        command_tx,
        OrchestratorCommand::Interrupt { turn_id },
        state,
    ) {
        state.status_message = Some(format!("{} #{turn_id}", i18n::text(Text::InterruptingTurn)));
    }
}

fn reset_from_modal(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if let Some(control) = urgent_control {
        control.reset();
        state.status_message = Some(i18n::text(Text::ResetRequested).to_owned());
    } else if try_send(command_tx, OrchestratorCommand::Reset, state) {
        state.status_message = Some(i18n::text(Text::ResetRequested).to_owned());
    }
}

fn insert_text(state: &mut AppState, text: &str) {
    clamp_cursor(state);
    state.input_buffer.insert_str(state.input_cursor, text);
    state.input_cursor = state.input_cursor.saturating_add(text.len());
}

fn backspace_grapheme(state: &mut AppState) {
    clamp_cursor(state);
    let previous = state.input_buffer[..state.input_cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index);
    if let Some(previous) = previous {
        state.input_buffer.drain(previous..state.input_cursor);
        state.input_cursor = previous;
    }
}

fn delete_grapheme(state: &mut AppState) {
    clamp_cursor(state);
    let next = state.input_buffer[state.input_cursor..]
        .graphemes(true)
        .next()
        .map(str::len);
    if let Some(length) = next {
        state
            .input_buffer
            .drain(state.input_cursor..state.input_cursor.saturating_add(length));
    }
}

fn move_left(state: &mut AppState) {
    clamp_cursor(state);
    if let Some((index, _)) = state.input_buffer[..state.input_cursor]
        .grapheme_indices(true)
        .next_back()
    {
        state.input_cursor = index;
    }
}

fn move_right(state: &mut AppState) {
    clamp_cursor(state);
    if let Some(grapheme) = state.input_buffer[state.input_cursor..]
        .graphemes(true)
        .next()
    {
        state.input_cursor = state.input_cursor.saturating_add(grapheme.len());
    }
}

fn move_line_start(state: &mut AppState) {
    clamp_cursor(state);
    state.input_cursor = state.input_buffer[..state.input_cursor]
        .rfind('\n')
        .map_or(0, |index| index.saturating_add(1));
}

fn move_line_end(state: &mut AppState) {
    clamp_cursor(state);
    state.input_cursor = state.input_buffer[state.input_cursor..]
        .find('\n')
        .map_or(state.input_buffer.len(), |relative| {
            state.input_cursor.saturating_add(relative)
        });
}

fn move_vertical(state: &mut AppState, down: bool) {
    clamp_cursor(state);
    let line_start = state.input_buffer[..state.input_cursor]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let column = UnicodeWidthStr::width(&state.input_buffer[line_start..state.input_cursor]);
    let target_start = if down {
        let current_end = state.input_buffer[state.input_cursor..]
            .find('\n')
            .map(|relative| state.input_cursor + relative);
        let Some(current_end) = current_end else {
            return;
        };
        current_end + 1
    } else {
        if line_start == 0 {
            return;
        }
        state.input_buffer[..line_start - 1]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    };
    let target_end = state.input_buffer[target_start..]
        .find('\n')
        .map_or(state.input_buffer.len(), |relative| target_start + relative);
    let mut width = 0;
    let mut cursor = target_start;
    for (relative, grapheme) in state.input_buffer[target_start..target_end].grapheme_indices(true)
    {
        let next_width = width + UnicodeWidthStr::width(grapheme);
        if next_width > column {
            break;
        }
        width = next_width;
        cursor = target_start + relative + grapheme.len();
    }
    state.input_cursor = cursor;
}

fn clamp_cursor(state: &mut AppState) {
    state.input_cursor = state.input_cursor.min(state.input_buffer.len());
    while !state.input_buffer.is_char_boundary(state.input_cursor) {
        state.input_cursor = state.input_cursor.saturating_sub(1);
    }
}

fn select_next_tool(state: &mut AppState) {
    let ids = tool_ids(state);
    if ids.is_empty() {
        return;
    }
    let index = state
        .selected_tool
        .and_then(|selected| ids.iter().position(|id| *id == selected))
        .map_or(0, |index| (index + 1) % ids.len());
    state.selected_tool = Some(ids[index]);
}

fn select_previous_tool(state: &mut AppState) {
    let ids = tool_ids(state);
    if ids.is_empty() {
        return;
    }
    let index = state
        .selected_tool
        .and_then(|selected| ids.iter().position(|id| *id == selected))
        .map_or(ids.len() - 1, |index| {
            index.checked_sub(1).unwrap_or(ids.len() - 1)
        });
    state.selected_tool = Some(ids[index]);
}

fn tool_ids(state: &AppState) -> Vec<u64> {
    let mut ids: Vec<_> = state
        .history
        .iter()
        .filter_map(|entry| match &entry.kind {
            crate::agent::state::HistoryKind::ToolResult { action_id, .. } => Some(*action_id),
            _ => None,
        })
        .chain(state.tool_actions.keys().copied())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn toggle_selected_tool(state: &mut AppState) -> bool {
    let Some(action_id) = state.selected_tool else {
        return false;
    };
    if !state.expanded_tools.remove(&action_id) {
        state.expanded_tools.insert(action_id);
    }
    true
}

fn submit_prompt(state: &mut AppState, command_tx: &mpsc::Sender<OrchestratorCommand>) {
    if !prepare_composer_attachments(state) {
        return;
    }
    let trimmed = state.input_buffer.trim();
    let follow_up_mode = if trimmed == "/queue" || trimmed.starts_with("/queue ") {
        Some((FollowUpMode::Queue, "/queue"))
    } else if trimmed == "/steer" || trimmed.starts_with("/steer ") {
        Some((FollowUpMode::Steer, "/steer"))
    } else {
        None
    };
    if let Some((mode, prefix)) = follow_up_mode {
        let text = trimmed
            .strip_prefix(prefix)
            .unwrap_or_default()
            .trim()
            .to_owned();
        state.input_buffer.clear();
        state.input_cursor = 0;
        open_follow_ups(state, true);
        state.follow_up_ui.push_text(&text);
        if !text.is_empty() {
            enqueue_follow_up(mode, state, command_tx);
        }
        return;
    }
    if trimmed == "/btw" || trimmed.starts_with("/btw ") {
        let question = trimmed
            .strip_prefix("/btw")
            .unwrap_or_default()
            .trim()
            .to_owned();
        open_side_chat(state);
        if !state.side_chat_ui.is_open() {
            return;
        }
        state.input_buffer.clear();
        state.input_cursor = 0;
        state.side_chat_ui.compose();
        state.side_chat_ui.push_text(&question);
        if !question.is_empty() {
            submit_side_question(state, command_tx);
        }
        return;
    }
    if state.phase.is_busy() {
        let pending = state.input_buffer.clone();
        state.input_buffer.clear();
        state.input_cursor = 0;
        open_follow_ups(state, true);
        state.follow_up_ui.push_text(&pending);
        state.status_message = Some(i18n::text(Text::QueueSteerHelp).to_owned());
        return;
    }
    if state.input_buffer.trim().is_empty() && state.pending_attachments.is_empty() {
        return;
    }
    let prompt = state.input_buffer.clone();
    let attachments = state
        .pending_attachments
        .iter()
        .map(|attachment| attachment.source.clone())
        .collect();
    let command = OrchestratorCommand::Submit {
        prompt,
        attachments,
        scope: CommandScope {
            conversation_epoch: state.conversation_epoch,
            phase_revision: state.phase_revision,
        },
    };
    if try_send(command_tx, command, state) {
        state.input_buffer.clear();
        state.input_cursor = 0;
        state.pending_attachments.clear();
        state.live_thinking.clear();
        state.live_assistant.clear();
        state.interrupted_draft.clear();
        state.status_message = Some(i18n::text(Text::SubmitInProgress).to_owned());
    }
}

fn attach_workspace_file(state: &mut AppState, path: String) {
    attach_draft(state, AttachmentDraft::from_workspace_path(path));
}

fn attach_draft(state: &mut AppState, attachment: AttachmentDraft) {
    if state.pending_attachments.len() >= MAX_ATTACHMENTS_PER_TURN {
        state.status_message = Some(format!(
            "{}: {MAX_ATTACHMENTS_PER_TURN}",
            i18n::text(Text::AttachmentHint)
        ));
        return;
    }
    if state
        .pending_attachments
        .iter()
        .any(|candidate| candidate.source == attachment.source)
    {
        state.status_message = Some(format!(
            "{}: {}",
            attachment.filename,
            i18n::text(Text::AlreadyRunning)
        ));
        return;
    }
    state.status_message = Some(format!(
        "{}: {} ({})",
        i18n::text(Text::AttachmentHint),
        attachment.filename,
        attachment.kind.label()
    ));
    state.pending_attachments.push(attachment);
}

fn interrupt_or_quit(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    if state.phase.is_busy() {
        if let Some(turn_id) = state.active_turn_id {
            let sent = if let Some(control) = urgent_control {
                control.interrupt(turn_id);
                true
            } else {
                try_send(
                    command_tx,
                    OrchestratorCommand::Interrupt { turn_id },
                    state,
                )
            };
            if sent {
                state.status_message =
                    Some(format!("{} #{turn_id}", i18n::text(Text::InterruptingTurn)));
            }
        } else {
            state.status_message = Some(i18n::text(Text::NoActiveTurn).to_owned());
        }
        return;
    }
    if let Some(control) = urgent_control {
        control.shutdown();
    } else {
        let _ = try_send(command_tx, OrchestratorCommand::Shutdown, state);
    }
    state.should_quit = true;
}

fn send_whip(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    urgent_control: Option<&UrgentControlHandle>,
) {
    let Some(turn_id) = state.active_turn_id else {
        return;
    };
    let sent = if let Some(control) = urgent_control {
        control.whip(turn_id);
        true
    } else {
        try_send(command_tx, OrchestratorCommand::Whip { turn_id }, state)
    };
    if sent {
        state.whip.record_request();
        state.status_message = Some(format!(
            "Whip · {} #{turn_id}",
            i18n::text(Text::RunningStatus)
        ));
    }
}

fn send_failed_turn_decision(
    state: &mut AppState,
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    retry: bool,
) {
    if !state.phase.is_error()
        || (retry
            && !matches!(
                state.phase,
                crate::agent::phase::AgentPhase::Error {
                    recoverable: true,
                    ..
                }
            ))
    {
        return;
    }
    let Some(turn_id) = state.active_turn_id else {
        state.status_message = Some(i18n::text(Text::SelectedItemUnavailable).to_owned());
        return;
    };
    let command = if retry {
        OrchestratorCommand::RetryTurn { turn_id }
    } else {
        OrchestratorCommand::AbortTurn { turn_id }
    };
    if try_send(command_tx, command, state) {
        if !retry {
            state.eta.cancel(turn_id);
        }
        state.status_message = Some(if retry {
            format!("{} #{turn_id}", i18n::text(Text::Retry))
        } else {
            format!("{} #{turn_id}", i18n::text(Text::Abort))
        });
    }
}

fn is_control_char(key: KeyEvent, character: char) -> bool {
    matches!(key.code, KeyCode::Char(value) if value.eq_ignore_ascii_case(&character))
        && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn try_send(
    command_tx: &mpsc::Sender<OrchestratorCommand>,
    command: OrchestratorCommand,
    state: &mut AppState,
) -> bool {
    match command_tx.try_send(command) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            state.status_message = Some(i18n::text(Text::BusyTurnRejected).to_owned());
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            state.status_message = Some(i18n::text(Text::ClosedStatus).to_owned());
            false
        }
    }
}

#[must_use]
pub fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use chrono::Utc;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use super::{
        handle_key, handle_lsp_hit, handle_menu_action, handle_privacy_hit, open_code_index,
        open_lsp, open_sessions, rect_contains,
    };
    use crate::{
        agent::{
            CheckpointSummary, OrchestratorCommand, SessionId, SessionSummary,
            SkillCatalogSnapshot, SkillSource, SkillSummary,
        },
        ui::app::AppState,
        ui::{lsp::LspHit, privacy::PrivacyHit},
    };
    use tokio::sync::mpsc;

    #[test]
    fn hitbox_includes_top_left_and_excludes_bottom_right() {
        let rect = Rect::new(10, 5, 4, 3);
        assert!(rect_contains(rect, 10, 5));
        assert!(rect_contains(rect, 13, 7));
        assert!(!rect_contains(rect, 14, 7));
        assert!(!rect_contains(rect, 13, 8));
        assert!(!rect_contains(rect, 9, 5));
    }

    #[test]
    fn backspace_removes_a_whole_grapheme_cluster() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = AppState::new();
        state.input_buffer = "a👩‍💻".to_owned();
        state.input_cursor = state.input_buffer.len();
        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        assert_eq!(state.input_buffer, "a");
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn failed_refresh_commands_keep_the_transport_error_visible() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);

        let mut sessions = AppState::new();
        open_sessions(&mut sessions, &tx);
        assert_eq!(sessions.status_message.as_deref(), Some("closed"));

        let mut lsp = AppState::new();
        open_lsp(&mut lsp, &tx);
        assert_eq!(lsp.status_message.as_deref(), Some("closed"));

        let mut code_index = AppState::new();
        open_code_index(&mut code_index, &tx);
        assert_eq!(code_index.status_message.as_deref(), Some("closed"));

        let mut privacy = AppState::new();
        privacy.privacy_ui.open(0);
        handle_privacy_hit(PrivacyHit::Reload, &mut privacy, &tx);
        assert_eq!(privacy.status_message.as_deref(), Some("closed"));

        let mut lsp_refresh = AppState::new();
        lsp_refresh.lsp_ui.open(0, 0);
        handle_lsp_hit(LspHit::Refresh, &mut lsp_refresh, &tx);
        assert_eq!(lsp_refresh.status_message.as_deref(), Some("closed"));
    }

    #[test]
    fn escape_from_rewind_picker_never_sends_a_command() {
        let (tx, mut rx) = mpsc::channel(2);
        let mut state = state_with_checkpoint();
        handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            &mut state,
            &tx,
        );
        assert!(state.rewind_ui.is_open());
        handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        assert!(!state.rewind_ui.is_open());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn rewind_requires_picker_then_explicit_confirmation() -> Result<(), Box<dyn std::error::Error>>
    {
        let (tx, mut rx) = mpsc::channel(2);
        let mut state = state_with_checkpoint();
        handle_key(
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL),
            &mut state,
            &tx,
        );
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        assert!(rx.try_recv().is_err());
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        let command = rx.try_recv()?;
        let OrchestratorCommand::Rewind { checkpoint_id, .. } = command else {
            return Err("unexpected command".into());
        };
        assert_eq!(checkpoint_id, 7);
        Ok(())
    }

    #[test]
    fn session_resume_is_clickable_and_workspace_mismatch_needs_second_confirmation()
    -> Result<(), Box<dyn std::error::Error>> {
        let (tx, mut rx) = mpsc::channel(4);
        let mut state = AppState::new();
        state.workspace_root = PathBuf::from("D:/current");
        state.sessions = Arc::from([SessionSummary {
            id: SessionId::parse("abc-123")?,
            title: "Other workspace".to_owned(),
            preview: "preview".to_owned(),
            workspace_root: PathBuf::from("D:/other"),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            pinned: false,
            archived: false,
            history_entries: 3,
            parent_session_id: None,
            recovered_records: 0,
        }]);

        handle_key(
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &mut state,
            &tx,
        );
        assert!(matches!(
            rx.try_recv()?,
            OrchestratorCommand::RefreshSessions { .. }
        ));
        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        assert!(matches!(
            state.session_ui.stage(),
            crate::ui::sessions::SessionStage::WorkspaceConfirm
        ));
        assert!(rx.try_recv().is_err());

        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        let command = rx.try_recv()?;
        let OrchestratorCommand::ResumeSession {
            allow_workspace_mismatch,
            ..
        } = command
        else {
            return Err("unexpected command".into());
        };
        assert!(allow_workspace_mismatch);
        assert!(!state.session_ui.is_open());
        Ok(())
    }

    #[test]
    fn skills_menu_opens_manager_and_space_sends_scoped_toggle()
    -> Result<(), Box<dyn std::error::Error>> {
        let (tx, mut rx) = mpsc::channel(2);
        let mut state = AppState::new();
        state.conversation_epoch = 9;
        state.phase_revision = 4;
        state.skills = SkillCatalogSnapshot {
            revision: 2,
            skills: Arc::from([SkillSummary {
                id: "project:review".to_owned(),
                name: "Review".to_owned(),
                description: "Review safely".to_owned(),
                source: SkillSource::Project,
                display_path: ".decode/skills/review/SKILL.md".to_owned(),
                enabled: true,
                resource_count: 0,
            }]),
            diagnostics: Arc::from([]),
            metadata_budget_bytes: 4_096,
            metadata_bytes_used: 512,
            metadata_omitted: 0,
        };

        handle_menu_action("skills", &mut state, &tx, None);
        assert!(state.skills_ui.is_open());
        handle_key(
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
            &mut state,
            &tx,
        );
        let OrchestratorCommand::SetSkillEnabled { id, enabled, scope } = rx.try_recv()? else {
            return Err("unexpected command".into());
        };
        assert_eq!(id, "project:review");
        assert!(!enabled);
        assert_eq!(scope.conversation_epoch, 9);
        assert_eq!(scope.phase_revision, 4);
        Ok(())
    }

    fn state_with_checkpoint() -> AppState {
        let mut state = AppState::new();
        state.checkpoints = std::sync::Arc::from([CheckpointSummary {
            id: 7,
            created_at: Utc::now(),
            prompt_preview: "edit".to_owned(),
            changed_paths: vec!["src/lib.rs".to_owned()],
            history_entries_before: 2,
            session_id: None,
        }]);
        state
    }
}
