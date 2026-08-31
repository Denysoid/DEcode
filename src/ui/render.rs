use std::time::{Duration, Instant};

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use ratatui_interact::components::{Button, ButtonState, ButtonStyle, ButtonVariant};
use similar::{ChangeTag, TextDiff};
use throbber_widgets_tui::{BRAILLE_SIX, Throbber, ThrobberState, WhichUse};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::{
    agent::{
        phase::AgentPhase,
        state::{HistoryKind, HistoryStatus, ToolResultStatus},
    },
    api::{ReasoningEffort, ReasoningMode},
    parser::tool_action::ToolAction,
    usage::{CostCoverage, UsageSnapshot, format_microusd},
};

use super::{
    app::AppState,
    code_index::state_label as code_index_state_label,
    confirm,
    i18n::{Text, text},
    mascot::MascotMood,
    modes::goal_status_label,
    shell::{ShellHit, ShellTab},
    syntax,
    whip::WhipDisplayKind,
};

const HISTORY_PREVIEW_GRAPHEMES: usize = 2_000;
const TOOL_PREVIEW_GRAPHEMES: usize = 140;
const TOOL_ARTIFACT_GRAPHEMES: usize = 8_000;
const TOOL_DETAIL_LINES: usize = 160;
const DRAFT_PREVIEW_GRAPHEMES: usize = 2_000;

const SERVICE_BLOCKS: [(&str, &str); 7] = [
    ("<thinking>", "</thinking>"),
    ("<read_file>", "</read_file>"),
    ("<list_directory>", "</list_directory>"),
    ("<search_code>", "</search_code>"),
    ("<apply_patch>", "</apply_patch>"),
    ("<write_file>", "</write_file>"),
    ("<execute_command>", "</execute_command>"),
];

pub fn draw(frame: &mut Frame<'_>, state: &mut AppState) {
    state.whip_hitbox = None;
    state.rewind_ui.begin_frame();
    state.session_ui.begin_frame();
    state.shell_ui.begin_frame();
    state.palette_ui.begin_frame();
    state.runtime_ui.begin_frame();
    state.mcp_ui.begin_frame();
    state.lsp_ui.begin_frame();
    state.code_index_ui.begin_frame();
    state.privacy_ui.begin_frame();
    state.permission_ui.begin_frame();
    state.usage_ui.begin_frame();
    state.side_chat_ui.begin_frame();
    state.follow_up_ui.begin_frame();
    state.review_ui.begin_frame();
    state.notification_ui.begin_frame();
    state.github_ui.begin_frame();
    state.modes_ui.begin_frame();
    state.instructions_ui.begin_frame();
    state.language_ui.begin_frame();
    state.skills_ui.begin_frame();
    state.automation_ui.begin_frame();
    state.plugin_ui.begin_frame();
    state.approval_center_ui.begin_frame();
    state.plan_approval_ui.begin_frame();
    state.agents_ui.begin_frame();
    state.patch_review_ui.begin_frame();
    if frame.area().width < 24 || frame.area().height < 8 {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        frame.render_widget(
            Paragraph::new(text(Text::TerminalTooSmall))
                .alignment(Alignment::Center)
                .wrap(Wrap { trim: false }),
            frame.area(),
        );
        return;
    }
    draw_main(frame, state);
    if let Some(pending) = state.pending_plan_review.as_ref() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.plan_approval_ui.draw(frame, &pending.review);
    } else if let Some(pending) = state.pending_confirmation.as_ref() {
        let max_scroll = confirm::draw_confirmation_dialog_scrolled(
            frame,
            pending,
            state.confirmation_scroll,
            state.confirmation_suffix_viewed,
            &mut state.confirmation_ui,
        );
        if let Some(max_scroll) = max_scroll {
            state.confirmation_view_ready = true;
            state.confirmation_max_scroll = max_scroll;
            state.confirmation_scroll = state.confirmation_scroll.min(max_scroll);
            if max_scroll == 0 {
                state.confirmation_suffix_viewed = true;
                state.confirmation_end_requested = false;
            } else if state.confirmation_end_requested && state.confirmation_scroll == max_scroll {
                // Only a frame that actually rendered the last hard-wrapped row
                // completes review. PageDown reaching the end never unlocks it.
                state.confirmation_suffix_viewed = true;
                state.confirmation_end_requested = false;
            }
        } else {
            state.confirmation_view_ready = false;
            state.confirmation_end_requested = false;
        }
    } else if let Some(pending) = state.pending_mcp_confirmation.as_ref() {
        let max_scroll = confirm::draw_mcp_confirmation_dialog_scrolled(
            frame,
            pending,
            state.confirmation_scroll,
            state.confirmation_suffix_viewed,
            &mut state.confirmation_ui,
        );
        if let Some(max_scroll) = max_scroll {
            state.confirmation_view_ready = true;
            state.confirmation_max_scroll = max_scroll;
            state.confirmation_scroll = state.confirmation_scroll.min(max_scroll);
            if max_scroll == 0
                || (state.confirmation_end_requested && state.confirmation_scroll == max_scroll)
            {
                state.confirmation_suffix_viewed = true;
                state.confirmation_end_requested = false;
            }
        } else {
            state.confirmation_view_ready = false;
            state.confirmation_end_requested = false;
        }
    } else if let Some(pending) = state.pending_patch_review.as_ref() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.patch_review_ui.draw(frame, &pending.review);
    } else if let Some(pending) = state.pending_subagent_review.as_ref() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        if let Some(review) = &pending.review.review {
            state.patch_review_ui.draw(frame, review);
        } else {
            state.agents_ui.draw_binary_review(frame, &pending.review);
        }
    } else if let Some(agent) = state
        .subagents
        .agents
        .iter()
        .find(|agent| agent.pending_command.is_some())
        .cloned()
    {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.agents_ui.draw_command_approval(frame, &agent);
    } else if let Some(pending) = state.pending_continuation {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        confirm::draw_continuation_dialog(frame, pending, &mut state.continuation_ui);
    } else if state.runtime_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.runtime_ui.draw(
            frame,
            state.deployment_choices.as_ref(),
            matches!(
                state.phase,
                AgentPhase::Idle
                    | AgentPhase::Error {
                        recoverable: true,
                        ..
                    }
            ),
        );
    } else if state.mcp_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.mcp_ui.draw(
            frame,
            state.mcp_servers.as_ref(),
            state.mcp_oauth_prompt.as_ref(),
            &state.subagents,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.lsp_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.lsp_ui.draw(
            frame,
            state.lsp_servers.as_ref(),
            state.lsp_diagnostics.as_ref(),
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.code_index_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state
            .code_index_ui
            .draw(frame, &state.code_index, state.code_index_hits.as_ref());
    } else if state.privacy_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state
            .privacy_ui
            .draw(frame, &state.privacy, state.phase == AgentPhase::Idle);
    } else if state.permission_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.permission_ui.draw(
            frame,
            &state.shell_permissions,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.usage_ui.is_open() {
        state.usage_ui.draw(
            frame,
            &state.usage,
            state.context_budget,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.follow_up_ui.is_open() {
        state.follow_up_ui.draw(
            frame,
            &state.follow_ups,
            state.phase.is_busy(),
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.review_ui.is_open() {
        state.review_ui.draw(
            frame,
            &state.reviews,
            matches!(state.phase, AgentPhase::Idle),
        );
    } else if state.notification_ui.is_open() {
        state.notification_ui.draw(frame, &state.notifications);
    } else if state.github_ui.is_open() {
        state.github_ui.draw(frame, &state.github);
    } else if state.side_chat_ui.is_open() {
        state
            .side_chat_ui
            .draw(frame, &state.side_chat, state.deployment_choices.as_ref());
    } else if state.modes_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.modes_ui.draw(
            frame,
            &state.work_modes,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.instructions_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.instructions_ui.draw(
            frame,
            &state.instructions,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.language_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.language_ui.draw(frame, state.language);
    } else if state.skills_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.skills_ui.draw(
            frame,
            &state.skills,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.automation_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.automation_ui.draw(frame, &state.automation);
    } else if state.plugin_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.plugin_ui.draw(
            frame,
            &state.plugins,
            matches!(state.phase, crate::agent::phase::AgentPhase::Idle),
        );
    } else if state.approval_center_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.approval_center_ui.draw(frame, state.auto_approval);
    } else if state.palette_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state
            .palette_ui
            .draw(frame, state.automation.commands.as_ref());
    } else if state.session_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.session_ui.draw_dialog(
            frame,
            state.sessions.as_ref(),
            state.current_session_id.as_ref(),
            matches!(state.phase, AgentPhase::Idle),
        );
    } else if state.rewind_ui.is_open() {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        state.rewind_ui.draw_dialog(
            frame,
            state.checkpoints.as_ref(),
            matches!(state.phase, AgentPhase::Idle),
        );
    } else if !matches!(state.agents_ui.editor(), super::agents::AgentEditor::Closed) {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
        let fleet = state.subagents.clone();
        let selected = state.agents_ui.selected(&fleet).cloned();
        state.agents_ui.draw_editor(
            frame,
            &fleet,
            selected.as_ref(),
            state.workspace_files.as_ref(),
        );
    } else {
        state.confirmation_view_ready = false;
        state.confirmation_end_requested = false;
    }
}

fn draw_main(frame: &mut Frame<'_>, state: &mut AppState) {
    let terminal_active = state.shell_ui.active_tab() == ShellTab::Terminal;
    let thinking_height =
        if terminal_active || !state.show_thinking || state.live_thinking.is_empty() {
            0
        } else {
            6
        };
    let editor_lines = state.input_buffer.lines().count().max(1) as u16;
    let input_height = if terminal_active {
        0
    } else {
        editor_lines
            .saturating_add(2)
            .saturating_add(u16::from(!state.pending_attachments.is_empty()))
            .clamp(3, 9)
    };
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(thinking_height),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ])
    .split(frame.area());
    state.shell_ui.draw_tabs(frame, chunks[1]);
    let content_area = super::shell::ShellUiState::content_area(chunks[1]);
    draw_shell_content(frame, content_area, state);
    if thinking_height > 0 {
        draw_thinking(frame, chunks[2], state);
    }
    if !terminal_active {
        draw_input(frame, chunks[3], state);
    }
    draw_status(frame, chunks[4], state);
    let idle = matches!(state.phase, AgentPhase::Idle);
    state.shell_ui.draw_menu(
        frame,
        frame.area(),
        idle,
        state.mascot.enabled(),
        state.show_thinking,
        state.show_tool_activity,
    );
    draw_brand_header(frame, state);
    state.shell_ui.draw_tool_menu(frame, frame.area());
}

fn draw_brand_header(frame: &mut Frame<'_>, state: &AppState) {
    if frame.area().width < 58 {
        return;
    }
    let working = state.phase.is_busy();
    let label = if working {
        let elapsed = state
            .eta
            .turn_elapsed(Instant::now())
            .unwrap_or_else(|| state.phase_started.elapsed());
        format!(
            "{} DEcode · {}  {}",
            animated_d(state),
            phase_label(&state.phase),
            format_duration(elapsed)
        )
    } else {
        format!("D  DEcode · {}", text(Text::Ready))
    };
    let width = u16::try_from(UnicodeWidthStr::width(label.as_str()))
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(frame.area().width / 2);
    let area = Rect::new(
        frame.area().right().saturating_sub(width),
        frame.area().y,
        width,
        1,
    );
    frame.render_widget(
        Paragraph::new(compact_single_line(&label, usize::from(width)))
            .alignment(Alignment::Right)
            .style(
                Style::default()
                    .fg(if working {
                        Color::LightCyan
                    } else {
                        Color::DarkGray
                    })
                    .bg(Color::Reset)
                    .add_modifier(Modifier::BOLD),
            ),
        area,
    );
}

const D_MORPH_FRAMES: [&str; 8] = ["ᴅ", "ᴅ", "D", "𝐃", "𝐃", "D", "ᴅ", "ᴅ"];

#[must_use]
pub const fn animated_d_frame(frame: usize) -> &'static str {
    D_MORPH_FRAMES[frame % D_MORPH_FRAMES.len()]
}

#[must_use]
pub fn animated_d(state: &AppState) -> &'static str {
    if !state.phase.is_busy() {
        return "D";
    }
    animated_d_frame(state.mascot.animation_frame())
}

#[must_use]
pub fn terminal_title(state: &AppState) -> String {
    if state.phase.is_busy() {
        format!(
            "{} DEcode · {}",
            animated_d(state),
            phase_label(&state.phase)
        )
    } else {
        format!("D DEcode · {}", text(Text::Ready))
    }
}

fn draw_shell_content(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let show_left = state.shell_ui.show_left_sidebar && area.width >= 92;
    // Keep the primary status/controls sidebar available on ordinary 100-column
    // terminals. The workspace sidebar still collapses independently, leaving
    // at least 30 columns for the active tab.
    let show_right = state.shell_ui.show_right_sidebar && area.width >= 82;
    let right_width = if show_right && area.width >= 150 {
        45
    } else {
        31
    };
    let columns = Layout::horizontal([
        Constraint::Length(if show_left { 25 } else { 0 }),
        Constraint::Min(30),
        Constraint::Length(if show_right { right_width } else { 0 }),
    ])
    .split(area);
    if show_left {
        draw_workspace_sidebar(frame, columns[0], state);
    }
    match state.shell_ui.active_tab() {
        ShellTab::Chat => draw_dialog(frame, columns[1], state),
        ShellTab::Activity => draw_activity(frame, columns[1], state),
        ShellTab::Diff => draw_diff_overview(frame, columns[1], state),
        ShellTab::Plan => draw_plan(frame, columns[1], state),
        ShellTab::Agents => draw_agents(frame, columns[1], state),
        ShellTab::Terminal => {
            let fleet = state.terminal.clone();
            state.terminal_ui.draw_tab(frame, columns[1], &fleet);
        }
    }
    if show_right {
        draw_side(frame, columns[2], state);
    }
}

fn draw_workspace_sidebar(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let sections =
        Layout::vertical([Constraint::Percentage(42), Constraint::Percentage(58)]).split(area);
    let session_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::Sessions)));
    let session_inner = session_block.inner(sections[0]);
    frame.render_widget(session_block, sections[0]);
    let session_rows = usize::from(session_inner.height);
    for (row, session) in state.sessions.iter().take(session_rows).enumerate() {
        let current = state.current_session_id.as_ref() == Some(&session.id);
        let marker = if current {
            "▶"
        } else if session.pinned {
            "★"
        } else {
            " "
        };
        let value = compact_single_line(&session.title, usize::from(session_inner.width).max(1));
        let row_area = Rect::new(
            session_inner.x,
            session_inner.y.saturating_add(row as u16),
            session_inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(format!("{marker} {value}")).style(if current {
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            }),
            row_area,
        );
        state
            .shell_ui
            .register_hit(row_area, ShellHit::Session(row));
    }

    let file_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", text(Text::WorkspaceFiles)));
    let file_inner = file_block.inner(sections[1]);
    frame.render_widget(file_block, sections[1]);
    let file_rows = usize::from(file_inner.height);
    for (row, path) in state.workspace_files.iter().take(file_rows).enumerate() {
        let safe = compact_single_line(path, usize::from(file_inner.width).saturating_sub(2));
        let row_area = Rect::new(
            file_inner.x,
            file_inner.y.saturating_add(row as u16),
            file_inner.width,
            1,
        );
        frame.render_widget(
            Paragraph::new(format!("  {safe}")).style(Style::default().fg(Color::DarkGray)),
            row_area,
        );
        state.shell_ui.register_hit(row_area, ShellHit::File(row));
    }
}

fn draw_dialog(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let title_body = if state.shell_ui.follow_output {
        text(Text::ChatFollowing).to_owned()
    } else {
        format!("{} • ↓ {}", text(Text::ChatPaused), text(Text::Latest))
    };
    let show_mascot = state.mascot.enabled() && area.width >= 29 && area.height >= 16;
    let title = if state.mascot.enabled() && !show_mascot {
        let mood = state.mascot.mood(&state.phase, Instant::now());
        format!(
            " {} {} · {title_body} ",
            text(Text::PixelName),
            state.mascot.mini_face(mood)
        )
    } else {
        format!(" {title_body} ")
    };
    let block = Block::default().borders(Borders::ALL).title(title.clone());
    if state.history.is_empty()
        && state.live_assistant.is_empty()
        && state.interrupted_draft.is_empty()
    {
        draw_welcome(frame, area, block, state);
        return;
    }
    let mut history_area = area;
    if show_mascot {
        let inner = block.inner(area);
        frame.render_widget(&block, area);
        let rows = Layout::vertical([Constraint::Length(10), Constraint::Min(4)]).split(inner);
        draw_mascot(frame, rows[0], state);
        history_area = rows[1];
    }
    // `scroll_offset` is an entry offset. Materialize only the entries that
    // can plausibly intersect this terminal viewport; the actor may retain a
    // bounded 2 MiB snapshot, but animation ticks must not re-sanitize all of
    // it on every frame.
    let viewport_entries = usize::from(history_area.height)
        .saturating_add(8)
        .clamp(1, 256);
    let max_start = state.history.len().saturating_sub(1);
    if state.shell_ui.follow_output {
        state.scroll_offset =
            u16::try_from(state.history.len().saturating_sub(viewport_entries)).unwrap_or(u16::MAX);
    } else {
        state.scroll_offset = state
            .scroll_offset
            .min(u16::try_from(max_start).unwrap_or(u16::MAX));
    }
    let start = usize::from(state.scroll_offset);
    let end = start
        .saturating_add(viewport_entries)
        .min(state.history.len());

    let mut lines = Vec::new();
    for entry in &state.history[start..end] {
        match &entry.kind {
            HistoryKind::User => {
                let mut content = entry.content.clone();
                for attachment in &entry.attachments {
                    if !content.is_empty() {
                        content.push('\n');
                    }
                    content.push_str("📎 ");
                    content.push_str(&attachment.history_label());
                }
                if state.active_turn_id == Some(entry.turn_id)
                    && matches!(entry.status, HistoryStatus::Pending)
                    && state.phase.is_busy()
                {
                    push_active_message(
                        &mut lines,
                        &format!("{} | {}", text(Text::You), text(Text::Pending)),
                        &content,
                        state.phase_started.elapsed(),
                    );
                } else {
                    push_message(
                        &mut lines,
                        text(Text::You),
                        &content,
                        Color::Cyan,
                        &entry.status,
                    );
                }
            }
            HistoryKind::Assistant => {
                let speaker = assistant_speaker(entry.epoch, entry.turn_id);
                if let Some(omitted) = context_compaction_count(&entry.content) {
                    lines.push(Line::from(Span::styled(
                        format!("{} · {omitted}", text(Text::ContextCompacted)),
                        Style::default()
                            .fg(Color::LightMagenta)
                            .add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(""));
                    continue;
                }
                if state.show_thinking {
                    for thinking in extract_thinking_blocks(&entry.content) {
                        push_branded_message(
                            &mut lines,
                            &format!("{speaker} · {}", text(Text::Thinking)),
                            thinking,
                            Color::DarkGray,
                            &entry.status,
                        );
                    }
                }
                let assistant = strip_service_blocks(&entry.content);
                if !assistant.is_empty() {
                    if let Some(metrics) = &entry.turn_metrics {
                        push_turn_metrics(&mut lines, metrics);
                    }
                    push_branded_message(
                        &mut lines,
                        speaker,
                        &assistant,
                        Color::LightCyan,
                        &entry.status,
                    );
                }
            }
            HistoryKind::ToolResult {
                action_id,
                tool_name,
                outcome,
            } if state.show_tool_activity => push_tool_result(
                &mut lines,
                *action_id,
                tool_name,
                outcome,
                &entry.content,
                state,
            ),
            HistoryKind::ToolResult { .. } => {}
        }
    }
    let viewport_reaches_tail = end == state.history.len();
    let current_speaker_turn = state
        .active_turn_id
        .or(state.paused_turn_id)
        .or_else(|| state.history.last().map(|entry| entry.turn_id))
        .unwrap_or_default();
    if viewport_reaches_tail && !state.live_assistant.is_empty() {
        push_branded_message(
            &mut lines,
            assistant_speaker(state.conversation_epoch, current_speaker_turn),
            &state.live_assistant,
            Color::Green,
            &HistoryStatus::Committed,
        );
    }
    if viewport_reaches_tail && !state.interrupted_draft.is_empty() {
        let speaker = assistant_speaker(state.conversation_epoch, current_speaker_turn);
        push_branded_message(
            &mut lines,
            &format!("{speaker} · {}", text(Text::InterruptedDraft)),
            &strip_service_blocks(&state.interrupted_draft),
            Color::DarkGray,
            &HistoryStatus::Interrupted,
        );
    }
    if viewport_reaches_tail
        && (state.phase.is_busy()
            || matches!(
                state.phase,
                AgentPhase::AwaitingConfirmation
                    | AgentPhase::AwaitingPatchApproval
                    | AgentPhase::AwaitingPlanApproval
                    | AgentPhase::AwaitingContinuation
            ))
    {
        push_current_activity(&mut lines, state);
    }
    let history = Paragraph::new(lines).wrap(Wrap { trim: false });
    let content_area = if show_mascot {
        history_area
    } else {
        block.inner(history_area)
    };
    let history_scroll = if state.shell_ui.follow_output {
        history
            .line_count(content_area.width.max(1))
            .saturating_sub(usize::from(content_area.height))
            .min(usize::from(u16::MAX)) as u16
    } else {
        0
    };
    let history = history.scroll((history_scroll, 0));
    if show_mascot {
        frame.render_widget(history, history_area);
    } else {
        frame.render_widget(
            history.block(Block::default().borders(Borders::ALL).title(title)),
            history_area,
        );
    }
    if !state.shell_ui.follow_output && history_area.width >= 16 {
        let latest = Rect::new(
            history_area.right().saturating_sub(14),
            history_area.y,
            13.min(history_area.width),
            1,
        );
        state.shell_ui.register_hit(latest, ShellHit::JumpLatest);
    }
}

fn draw_mascot(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let mood = state.mascot.mood(&state.phase, Instant::now());
    frame.render_widget(
        Paragraph::new(state.mascot.art(mood).join("\n"))
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · {} ",
                text(Text::PixelName),
                state.mascot.mini_face(mood)
            )))
            .alignment(Alignment::Center)
            .style(Style::default().fg(mascot_color(mood))),
        area,
    );
}

const fn mascot_color(mood: MascotMood) -> Color {
    match mood {
        MascotMood::Working => Color::LightCyan,
        MascotMood::Celebrating
        | MascotMood::VictoryRolling
        | MascotMood::Playful
        | MascotMood::Pouncing
        | MascotMood::Affectionate
        | MascotMood::Purring
        | MascotMood::Dancing
        | MascotMood::Stretching => Color::LightGreen,
        MascotMood::Error => Color::LightRed,
        MascotMood::Sleeping | MascotMood::Yawning => Color::DarkGray,
        MascotMood::Hungry | MascotMood::Overfed => Color::LightYellow,
        MascotMood::Waiting | MascotMood::Burping => Color::LightMagenta,
        MascotMood::Curious
        | MascotMood::Blinking
        | MascotMood::Waving
        | MascotMood::Grooming
        | MascotMood::Rolling
        | MascotMood::Chasing
        | MascotMood::Stargazing
        | MascotMood::Tongue => Color::LightBlue,
    }
}

fn draw_welcome(frame: &mut Frame<'_>, area: Rect, block: Block<'_>, state: &mut AppState) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if !state.mascot.enabled() || inner.height < 15 {
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    text(Text::WelcomeTitle),
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(text(Text::WelcomeBody)),
            ])
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let rows = Layout::vertical([
        Constraint::Length(10),
        Constraint::Length(2),
        Constraint::Min(2),
        Constraint::Length(3),
    ])
    .split(inner);
    let now = Instant::now();
    let mood = state.mascot.mood(&state.phase, now);
    let art = state.mascot.art(mood);
    frame.render_widget(
        Paragraph::new(art.join("\n"))
            .alignment(Alignment::Center)
            .style(Style::default().fg(mascot_color(mood))),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(text(Text::WelcomeTitle))
            .alignment(Alignment::Center)
            .style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(text(Text::WelcomeBody))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: false }),
        rows[2],
    );
    let feed_label = format!("{} (F7)", text(Text::Feed));
    let wake_label = text(Text::Wake).to_owned();
    let feed_width = button_content_width(&feed_label);
    let wake_width = button_content_width(&wake_label);
    let buttons = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Length(feed_width),
        Constraint::Length(2),
        Constraint::Length(wake_width),
        Constraint::Fill(1),
    ])
    .split(rows[3]);
    let feed_label = fit_button_label(&feed_label, buttons[1]);
    let wake_label = fit_button_label(&wake_label, buttons[3]);
    let feed_state = if state.phase.is_busy() {
        ButtonState::disabled()
    } else {
        ButtonState::enabled()
    };
    let feed = Button::new(&feed_label, &feed_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::primary());
    let feed_region = feed.render_stateful(buttons[1], frame.buffer_mut());
    if !state.phase.is_busy() {
        state
            .shell_ui
            .register_hit(feed_region.area, ShellHit::MascotFeed);
    }

    let wake_state = ButtonState::enabled();
    let wake = Button::new(&wake_label, &wake_state)
        .variant(ButtonVariant::Block)
        .style(if mood == MascotMood::Sleeping {
            ButtonStyle::success()
        } else {
            ButtonStyle::default()
        });
    let wake_region = wake.render_stateful(buttons[3], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(wake_region.area, ShellHit::MascotWake);
}

fn draw_activity(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let running_count = state.running_tools.len();
    let title = if running_count > 1 {
        format!(
            " {} · {running_count} {} ",
            text(Text::Activity),
            text(Text::ActivityParallelHint)
        )
    } else {
        format!(" {} ", text(Text::ActivityHint))
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let capacity = usize::from(inner.height / 3).max(1);
    let mut cards = state
        .running_tools
        .iter()
        .rev()
        .filter_map(|action_id| {
            state.tool_actions.get(action_id).map(|action| {
                (
                    *action_id,
                    action.tool_name().to_owned(),
                    None,
                    if running_count > 1 {
                        text(Text::RunningConcurrent).to_owned()
                    } else {
                        text(Text::RunningTool).to_owned()
                    },
                )
            })
        })
        .take(capacity)
        .collect::<Vec<_>>();
    let remaining = capacity.saturating_sub(cards.len());
    cards.extend(
        state
            .history
            .iter()
            .rev()
            .filter_map(|entry| match &entry.kind {
                HistoryKind::ToolResult {
                    action_id,
                    tool_name,
                    outcome,
                } if !state.running_tools.contains(action_id) => Some((
                    *action_id,
                    tool_name.clone(),
                    Some(outcome.clone()),
                    entry.content.clone(),
                )),
                HistoryKind::ToolResult { .. } | HistoryKind::User | HistoryKind::Assistant => None,
            })
            .take(remaining),
    );
    let spinner =
        ["◐D", "◓D", "◑D", "◒D"][(state.phase_started.elapsed().as_millis() / 125) as usize % 4];
    for (index, (action_id, tool_name, outcome, content)) in cards.into_iter().enumerate() {
        let y = inner.y.saturating_add((index as u16).saturating_mul(3));
        if y >= inner.bottom() {
            break;
        }
        let card = Rect::new(
            inner.x,
            y,
            inner.width,
            3.min(inner.bottom().saturating_sub(y)),
        );
        let (status, color) = outcome
            .as_ref()
            .map_or((text(Text::RunningTool), Color::LightCyan), tool_status);
        let selected = state.selected_tool == Some(action_id);
        let marker = if outcome.is_some() { "#" } else { spinner };
        let title = format!(
            " {marker} #{action_id} {} | {status} ",
            sanitize_for_display(&tool_name)
        );
        frame.render_widget(
            Paragraph::new(compact_single_line(&content, 220))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(title)
                        .border_style(if selected {
                            Style::default().fg(Color::LightCyan)
                        } else {
                            Style::default().fg(color)
                        }),
                )
                .style(Style::default().fg(Color::Gray)),
            card,
        );
        state.shell_ui.register_hit(card, ShellHit::Tool(action_id));
    }
    if state.running_tools.is_empty()
        && state
            .history
            .iter()
            .all(|entry| !matches!(entry.kind, HistoryKind::ToolResult { .. }))
    {
        frame.render_widget(
            Paragraph::new(text(Text::NoToolActivity))
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn draw_diff_overview(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(text(Text::DiffReviewOverview));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let patches = state
        .tool_actions
        .iter()
        .rev()
        .filter_map(|(action_id, action)| match action {
            ToolAction::ApplyPatch {
                path,
                search,
                replace,
            } => Some((*action_id, path, search, replace)),
            _ => None,
        })
        .take(usize::from(inner.height / 4).max(1))
        .collect::<Vec<_>>();
    for (index, (action_id, path, search, replace)) in patches.into_iter().enumerate() {
        let y = inner.y.saturating_add((index as u16).saturating_mul(4));
        if y >= inner.bottom() {
            break;
        }
        let card = Rect::new(
            inner.x,
            y,
            inner.width,
            4.min(inner.bottom().saturating_sub(y)),
        );
        let diff = TextDiff::from_lines(search, replace);
        let additions = diff
            .iter_all_changes()
            .filter(|change| matches!(change.tag(), ChangeTag::Insert))
            .count();
        let deletions = diff
            .iter_all_changes()
            .filter(|change| matches!(change.tag(), ChangeTag::Delete))
            .count();
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(format!("{} #{action_id}", text(Text::Diff))),
                Line::from(vec![
                    Span::styled(format!("+{additions}"), Style::default().fg(Color::Green)),
                    Span::raw("  "),
                    Span::styled(format!("-{deletions}"), Style::default().fg(Color::Red)),
                ]),
            ])
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" ✎ {} ", compact_single_line(path, 160))),
            ),
            card,
        );
    }
    if state
        .tool_actions
        .values()
        .all(|action| !matches!(action, ToolAction::ApplyPatch { .. }))
    {
        frame.render_widget(
            Paragraph::new(text(Text::NoPatchesSession))
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
    }
}

fn draw_plan(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let latest_prompt = state.history.iter().rev().find_map(|entry| {
        matches!(entry.kind, HistoryKind::User).then(|| compact_single_line(&entry.content, 320))
    });
    let completed_tools = state
        .history
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                HistoryKind::ToolResult {
                    outcome: ToolResultStatus::Success,
                    ..
                }
            )
        })
        .count();
    let failed_tools = state
        .history
        .iter()
        .filter(|entry| {
            matches!(
                entry.kind,
                HistoryKind::ToolResult {
                    outcome: ToolResultStatus::Failure | ToolResultStatus::ParseError,
                    ..
                }
            )
        })
        .count();
    let (effective_effort, effective_mode) =
        state.work_modes.effective_reasoning(state.reasoning_effort);
    let mode_summary = format!(
        "{}: {} · {} {} · {} {} · {} {} · {} {} · {} {}",
        text(Text::ActiveTogether),
        state.work_modes.active_summary(),
        text(Text::Plan),
        on_off(state.work_modes.plan),
        text(Text::ExploreLabel),
        on_off(state.work_modes.explore),
        text(Text::ReviewLabel),
        on_off(state.work_modes.review),
        text(Text::Goal),
        on_off(state.work_modes.goal_enabled()),
        text(Text::DeepThinkingLabel),
        on_off(state.work_modes.deep_thinking)
    );
    let (objective, progress) = state.work_modes.goal.as_ref().map_or_else(
        || {
            (
                latest_prompt.unwrap_or_else(|| text(Text::NoPersistentGoal).to_owned()),
                text(Text::GoalModeOffHelp).to_owned(),
            )
        },
        |goal| {
            (
                truncate_for_display(&sanitize_for_display(&goal.objective), 2_000),
                format!(
                    "{} · {} {} · {} {} · {} {} · {} {}\n{}",
                    goal_status_label(goal.status),
                    text(Text::Revision),
                    goal.revision,
                    goal.completed_steps.len(),
                    text(Text::CompleteLabel),
                    goal.next_steps.len(),
                    text(Text::NextLabel),
                    text(Text::CheckedTurn),
                    goal.last_checked_turn
                        .map_or_else(|| "—".to_owned(), |turn| turn.to_string()),
                    truncate_for_display(&sanitize_for_display(&goal.summary), 2_000)
                ),
            )
        },
    );
    let reasoning = effective_mode.map_or_else(
        || reasoning_effort_label(effective_effort).to_owned(),
        |mode| {
            format!(
                "{}/{}",
                reasoning_effort_label(effective_effort),
                reasoning_mode_label(mode)
            )
        },
    );
    let lines = vec![
        Line::from(Span::styled(
            text(Text::CurrentObjective),
            Style::default()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(objective),
        Line::from(""),
        Line::from(Span::styled(
            mode_summary,
            Style::default().fg(Color::Green),
        )),
        Line::from(format!("{}  {reasoning}", text(Text::EffectiveReasoning))),
        Line::from(progress),
        Line::from(""),
        Line::from(format!(
            "● {}       {}",
            text(Text::AgentPhaseLabel),
            sanitize_for_display(&state.phase.to_string())
        )),
        Line::from(format!(
            "✓ {}  {completed_tools}",
            text(Text::SuccessfulTools)
        )),
        Line::from(format!("! {}  {failed_tools}", text(Text::FailedTools))),
        Line::from(format!(
            "◆ {}  {}",
            text(Text::CheckpointsLabel),
            state.checkpoints.len()
        )),
        Line::from(""),
        Line::from(Span::styled(
            text(Text::PlanDurableHelp),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", text(Text::Plan))),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn on_off(value: bool) -> &'static str {
    if value {
        text(Text::OnLabel)
    } else {
        text(Text::OffLabel)
    }
}

fn draw_agents(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let fleet = state.subagents.clone();
    let main_idle = matches!(state.phase, AgentPhase::Idle);
    state.agents_ui.draw_tab(frame, area, &fleet, main_idle);
}

fn tool_status(outcome: &ToolResultStatus) -> (&'static str, Color) {
    match outcome {
        ToolResultStatus::Success => (text(Text::Completed), Color::Green),
        ToolResultStatus::Failure => (text(Text::Failed), Color::Red),
        ToolResultStatus::Declined => (text(Text::Declined), Color::Yellow),
        ToolResultStatus::ParseError => (text(Text::ParseError), Color::Magenta),
    }
}

fn phase_label(phase: &AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Idle => text(Text::Ready),
        AgentPhase::PreparingReview => text(Text::CapturingReviewSnapshot),
        AgentPhase::Planning => text(Text::BuildingImplementationPlan),
        AgentPhase::AwaitingPlanApproval => text(Text::WaitingPlanApproval),
        AgentPhase::Requesting => text(Text::SendingRequest),
        AgentPhase::Streaming => text(Text::WritingResponse),
        AgentPhase::Parsing => text(Text::ValidatingCompletedResponse),
        AgentPhase::ExecutingTools => text(Text::ExecutingTools),
        AgentPhase::AwaitingPatchApproval => text(Text::WaitingPatchApproval),
        AgentPhase::AwaitingConfirmation => text(Text::WaitingCommandApproval),
        AgentPhase::AwaitingContinuation => text(Text::WaitingContinuationApproval),
        AgentPhase::Error { .. } => text(Text::ErrorLabel),
    }
}

fn connection_status_label(status: &str) -> String {
    match status {
        "capturing review snapshot" => text(Text::CapturingReviewSnapshot).to_owned(),
        "planning (read-only)" => text(Text::BuildingImplementationPlan).to_owned(),
        "awaiting plan approval" => text(Text::WaitingPlanApproval).to_owned(),
        "WebSocket connecting" => {
            format!("WebSocket · {}", text(Text::ConnectingEllipsis))
        }
        "SSE connecting" => format!("SSE · {}", text(Text::ConnectingEllipsis)),
        "WebSocket streaming" => format!("WebSocket · {}", text(Text::WritingResponse)),
        "SSE streaming" => format!("SSE · {}", text(Text::WritingResponse)),
        "parsing" => text(Text::ValidatingCompletedResponse).to_owned(),
        "awaiting patch approval" => text(Text::WaitingPatchApproval).to_owned(),
        "awaiting confirmation" => text(Text::WaitingCommandApproval).to_owned(),
        "executing tool" => text(Text::ExecutingTools).to_owned(),
        "awaiting continuation" => text(Text::WaitingContinuationApproval).to_owned(),
        "reconnecting WebSocket" => format!("WebSocket · {}", text(Text::RetryScheduled)),
        "reconnecting SSE" => format!("SSE · {}", text(Text::RetryScheduled)),
        "idle" => text(Text::Ready).to_owned(),
        "error" => text(Text::ErrorLabel).to_owned(),
        other => sanitize_for_display(other),
    }
}

fn reasoning_effort_label(effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::Low => text(Text::Low),
        ReasoningEffort::Medium => text(Text::Medium),
        ReasoningEffort::High => text(Text::High),
        ReasoningEffort::XHigh => text(Text::XHigh),
        ReasoningEffort::Max => text(Text::Max),
    }
}

fn reasoning_mode_label(mode: ReasoningMode) -> &'static str {
    match mode {
        ReasoningMode::Standard => text(Text::Standard),
        ReasoningMode::Pro => "Pro",
    }
}

fn context_mode_label(mode: &str) -> &str {
    match mode {
        "stateless" => text(Text::ContextStateless),
        "stateful" => text(Text::ContextStateful),
        other => other,
    }
}

fn push_message<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    content: &str,
    color: Color,
    status: &HistoryStatus,
) {
    push_message_with_colors(lines, label, content, color, color, status);
}

fn push_branded_message<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    content: &str,
    content_color: Color,
    status: &HistoryStatus,
) {
    push_message_with_colors(lines, label, content, Color::Gray, content_color, status);
}

fn push_message_with_colors<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    content: &str,
    label_color: Color,
    content_color: Color,
    status: &HistoryStatus,
) {
    let status_suffix = match status {
        HistoryStatus::Committed => None,
        HistoryStatus::Pending => Some(text(Text::Pending)),
        HistoryStatus::Paused => Some(text(Text::Paused)),
        HistoryStatus::Interrupted => Some(text(Text::Interrupted)),
        HistoryStatus::Superseded => Some(text(Text::Superseded)),
        HistoryStatus::Failed => Some(text(Text::Failed)),
        HistoryStatus::Cancelled => Some(text(Text::Cancelled)),
    };
    let safe_label = sanitize_for_display(label);
    let label = status_suffix.map_or(safe_label.clone(), |suffix| {
        format!("{safe_label} | {suffix}")
    });
    let mut label_style = Style::default().fg(label_color);
    let mut content_style = Style::default().fg(content_color);
    match status {
        HistoryStatus::Committed => {}
        HistoryStatus::Pending => {
            label_style = label_style.fg(Color::Yellow);
            content_style = content_style.fg(Color::Yellow);
        }
        HistoryStatus::Paused => {
            label_style = label_style
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD);
            content_style = content_style
                .fg(Color::LightBlue)
                .add_modifier(Modifier::BOLD);
        }
        HistoryStatus::Interrupted | HistoryStatus::Superseded | HistoryStatus::Cancelled => {
            label_style = label_style.fg(Color::DarkGray).add_modifier(Modifier::DIM);
            content_style = content_style
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM);
        }
        HistoryStatus::Failed => {
            label_style = label_style.fg(Color::Red);
            content_style = content_style.fg(Color::Red);
        }
    }
    lines.push(Line::from(Span::styled(
        label,
        label_style.add_modifier(Modifier::BOLD),
    )));
    let display = truncate_for_display(&sanitize_for_display(content), HISTORY_PREVIEW_GRAPHEMES);
    if display.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("({})", text(Text::Empty)),
            content_style,
        )));
    } else {
        lines.extend(
            display
                .lines()
                .map(|line| Line::from(Span::styled(line.to_owned(), content_style))),
        );
    }
    lines.push(Line::from(""));
}

fn assistant_speaker(epoch: u64, turn_id: u64) -> &'static str {
    let mut value = epoch ^ turn_id.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    if (value ^ (value >> 31)).is_multiple_of(5) {
        "Pixel"
    } else {
        "DEcode"
    }
}

fn context_compaction_count(content: &str) -> Option<usize> {
    let (count, remainder) = content.strip_prefix('[')?.split_once(' ')?;
    if !remainder.starts_with("older history entries ")
        || !(remainder.contains("compacted") || remainder.contains("summarized"))
    {
        return None;
    }
    count.parse().ok()
}

fn push_turn_metrics<'a>(lines: &mut Vec<Line<'a>>, metrics: &crate::agent::state::TurnMetrics) {
    let cost = metrics
        .cost_microusd
        .map_or_else(|| text(Text::Unpriced).to_owned(), format_microusd);
    lines.push(Line::from(Span::styled(
        format!(
            "{}: {} · {}: {} ({}/{}) · {}: {cost}",
            text(Text::Elapsed),
            format_duration(Duration::from_millis(metrics.elapsed_millis)),
            text(Text::Tokens),
            metrics.total_tokens,
            metrics.input_tokens,
            metrics.output_tokens,
            text(Text::Cost),
        ),
        Style::default().fg(Color::DarkGray),
    )));
}

fn push_active_message<'a>(
    lines: &mut Vec<Line<'a>>,
    label: &str,
    content: &str,
    elapsed: Duration,
) {
    lines.push(shimmer_line(
        &sanitize_for_display(label),
        elapsed,
        Color::Yellow,
    ));
    let display = truncate_for_display(&sanitize_for_display(content), HISTORY_PREVIEW_GRAPHEMES);
    if display.is_empty() {
        lines.push(shimmer_line(
            &format!("({})", text(Text::Empty)),
            elapsed,
            Color::Yellow,
        ));
    } else {
        lines.extend(
            display
                .lines()
                .map(|line| shimmer_line(line, elapsed, Color::Yellow)),
        );
    }
    lines.push(Line::from(""));
}

fn push_current_activity<'a>(lines: &mut Vec<Line<'a>>, state: &AppState) {
    let detail = state
        .running_tools
        .iter()
        .next_back()
        .and_then(|action_id| state.tool_actions.get(action_id))
        .map_or_else(
            || match state.phase {
                AgentPhase::PreparingReview => text(Text::CapturingReviewSnapshot).to_owned(),
                AgentPhase::Planning => text(Text::BuildingImplementationPlan).to_owned(),
                AgentPhase::Requesting => text(Text::SendingRequest).to_owned(),
                AgentPhase::Streaming if !state.live_assistant.is_empty() => {
                    text(Text::WritingResponse).to_owned()
                }
                AgentPhase::Streaming if !state.live_thinking.is_empty() => {
                    text(Text::ReasoningNextStep).to_owned()
                }
                AgentPhase::Streaming => text(Text::WaitingFirstToken).to_owned(),
                AgentPhase::Parsing => text(Text::ValidatingCompletedResponse).to_owned(),
                AgentPhase::ExecutingTools => text(Text::ExecutingTools).to_owned(),
                AgentPhase::AwaitingPlanApproval => text(Text::WaitingPlanApproval).to_owned(),
                AgentPhase::AwaitingPatchApproval => text(Text::WaitingPatchApproval).to_owned(),
                AgentPhase::AwaitingConfirmation => text(Text::WaitingCommandApproval).to_owned(),
                AgentPhase::AwaitingContinuation => {
                    text(Text::WaitingContinuationApproval).to_owned()
                }
                AgentPhase::Idle | AgentPhase::Error { .. } => state.phase.to_string(),
            },
            |action| format!("{} {}", text(Text::RunningTool), action.tool_name()),
        );
    let safe = sanitize_for_display(&format!("• {detail}"));
    lines.push(shimmer_line(
        &safe,
        state.phase_started.elapsed(),
        Color::DarkGray,
    ));
}

fn shimmer_line<'a>(value: &str, elapsed: Duration, base: Color) -> Line<'a> {
    let graphemes = UnicodeSegmentation::graphemes(value, true).collect::<Vec<_>>();
    if graphemes.is_empty() {
        return Line::from("");
    }
    let cycle = graphemes.len().saturating_mul(2).saturating_sub(2).max(1);
    let step = usize::try_from(elapsed.as_millis() / 75).unwrap_or(usize::MAX) % cycle;
    let center = if step < graphemes.len() {
        step
    } else {
        cycle.saturating_sub(step)
    };
    Line::from(
        graphemes
            .into_iter()
            .enumerate()
            .map(|(index, grapheme)| {
                let distance = index.abs_diff(center);
                let style = match distance {
                    0 => Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                    1 => Style::default().fg(Color::LightCyan),
                    2 => Style::default().fg(Color::Cyan),
                    _ => Style::default().fg(base),
                };
                Span::styled(grapheme.to_owned(), style)
            })
            .collect::<Vec<_>>(),
    )
}

fn push_tool_result<'a>(
    lines: &mut Vec<Line<'a>>,
    action_id: u64,
    tool_name: &str,
    outcome: &ToolResultStatus,
    content: &str,
    state: &AppState,
) {
    let (status, color) = match outcome {
        ToolResultStatus::Success => (text(Text::Completed), Color::Green),
        ToolResultStatus::Failure => (text(Text::Failed), Color::Red),
        ToolResultStatus::Declined => (text(Text::Declined), Color::Yellow),
        ToolResultStatus::ParseError => (text(Text::ParseError), Color::Magenta),
    };
    let preview_limit = if content.starts_with("[tool result compacted:") {
        TOOL_ARTIFACT_GRAPHEMES
    } else {
        TOOL_PREVIEW_GRAPHEMES
    };
    let preview = compact_single_line(content, preview_limit);
    let selected = state.selected_tool == Some(action_id);
    let expanded = state.expanded_tools.contains(&action_id);
    let marker = if selected { ">" } else { " " };
    let disclosure = if expanded { "v" } else { ">" };
    let suffix = if preview.is_empty() {
        String::new()
    } else {
        format!(" | {preview}")
    };
    let mut row_style = Style::default().fg(color);
    if selected {
        row_style = row_style.add_modifier(Modifier::REVERSED);
    }
    lines.push(Line::from(vec![
        Span::styled(
            format!(
                "{marker}{disclosure} {} #{action_id} {}",
                text(Text::ToolLabel),
                sanitize_for_display(tool_name)
            ),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" [{status}]{suffix}"), row_style),
    ]));
    if !expanded {
        return;
    }
    if let Some(call) = state.mcp_calls.get(&action_id) {
        lines.push(Line::from(Span::styled(
            format!(
                "    MCP {}::{} · {} {} · {} {}",
                sanitize_for_display(&call.server),
                sanitize_for_display(&call.tool),
                text(Text::FunctionLabel),
                sanitize_for_display(&call.function_name),
                text(Text::CallIdLabel),
                sanitize_for_display(&call.call_id),
            ),
            Style::default().fg(Color::LightMagenta),
        )));
        let raw = serde_json::to_string_pretty(&call.arguments)
            .unwrap_or_else(|error| format!("{{\"preview_error\":\"{error}\"}}"));
        // Untrusted MCP text is sanitized before syntax highlighting.
        let safe = truncate_for_display(&sanitize_for_display(&raw), TOOL_ARTIFACT_GRAPHEMES);
        if let Some(highlighted) = syntax::highlight_source("arguments.json", &safe) {
            for row in highlighted.into_iter().take(TOOL_DETAIL_LINES) {
                let mut spans = vec![Span::raw("    ")];
                spans.extend(row);
                lines.push(Line::from(spans));
            }
        } else {
            lines.extend(safe.lines().take(TOOL_DETAIL_LINES).map(|line| {
                Line::from(Span::styled(
                    format!("    {line}"),
                    Style::default().fg(Color::Gray),
                ))
            }));
        }
    }
    if let Some(action) = state.tool_actions.get(&action_id) {
        push_action_detail(lines, action);
    }
    let artifact = truncate_for_display(&sanitize_for_display(content), TOOL_ARTIFACT_GRAPHEMES);
    for line in artifact.lines().take(TOOL_DETAIL_LINES) {
        lines.push(Line::from(Span::styled(
            format!("    {line}"),
            Style::default().fg(Color::Gray),
        )));
    }
}

fn push_action_detail<'a>(lines: &mut Vec<Line<'a>>, action: &ToolAction) {
    match action {
        ToolAction::ApplyPatch {
            path,
            search,
            replace,
        } => {
            lines.push(Line::from(Span::styled(
                format!("    apply_patch {}", compact_single_line(path, 180)),
                Style::default().fg(Color::Cyan),
            )));
            let safe_search =
                truncate_for_display(&sanitize_for_display(search), TOOL_ARTIFACT_GRAPHEMES);
            let safe_replace =
                truncate_for_display(&sanitize_for_display(replace), TOOL_ARTIFACT_GRAPHEMES);
            let search_highlights = syntax::highlight_source(path, &safe_search);
            let replace_highlights = syntax::highlight_source(path, &safe_replace);
            let diff = TextDiff::from_lines(&safe_search, &safe_replace);
            for change in diff.iter_all_changes().take(TOOL_DETAIL_LINES) {
                let (sign, color, tint, highlighted) = match change.tag() {
                    ChangeTag::Delete => (
                        "-",
                        Color::Red,
                        Some(Color::Rgb(45, 12, 18)),
                        change.old_index().and_then(|index| {
                            search_highlights
                                .as_ref()
                                .and_then(|lines| lines.get(index))
                        }),
                    ),
                    ChangeTag::Insert => (
                        "+",
                        Color::Green,
                        Some(Color::Rgb(10, 38, 25)),
                        change.new_index().and_then(|index| {
                            replace_highlights
                                .as_ref()
                                .and_then(|lines| lines.get(index))
                        }),
                    ),
                    ChangeTag::Equal => (
                        " ",
                        Color::DarkGray,
                        None,
                        change.new_index().and_then(|index| {
                            replace_highlights
                                .as_ref()
                                .and_then(|lines| lines.get(index))
                        }),
                    ),
                };
                let value = truncate_for_display(change.value().trim_end_matches('\n'), 1_000);
                let mut spans = vec![Span::styled(
                    format!("    {sign}"),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )];
                if value == change.value().trim_end_matches('\n') {
                    if let Some(highlighted) = highlighted {
                        spans.extend(highlighted.iter().cloned().map(|mut span| {
                            if let Some(background) = tint {
                                span.style = span.style.bg(background);
                            }
                            span
                        }));
                    } else {
                        spans.push(Span::styled(value, Style::default().fg(color)));
                    }
                } else {
                    spans.push(Span::styled(value, Style::default().fg(color)));
                }
                lines.push(Line::from(spans));
            }
        }
        ToolAction::ReadFile { path } => push_summary(lines, "read", path),
        ToolAction::ListDirectory { path } => push_summary(lines, "list", path),
        ToolAction::SearchCode { pattern, path } => push_summary(
            lines,
            "search",
            &format!("{} in {}", pattern, path.as_deref().unwrap_or(".")),
        ),
        ToolAction::WriteFile { path, content } => push_summary(
            lines,
            "write",
            &format!("{} ({} bytes)", path, content.len()),
        ),
        ToolAction::ExecuteCommand { command, .. } => push_summary(lines, "command", command),
    }
}

fn push_summary<'a>(lines: &mut Vec<Line<'a>>, kind: &str, value: &str) {
    lines.push(Line::from(Span::styled(
        format!("    {kind}: {}", compact_single_line(value, 320)),
        Style::default().fg(Color::Cyan),
    )));
}

fn draw_thinking(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let thinking = truncate_for_display(
        &sanitize_for_display(&state.live_thinking),
        DRAFT_PREVIEW_GRAPHEMES,
    );
    frame.render_widget(
        Paragraph::new(thinking)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(text(Text::LiveThinkingTemporary)),
            )
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_side(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let expanded_rule_controls = area.width >= 37;
    let side_chunks = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(if expanded_rule_controls { 0 } else { 3 }),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(if expanded_rule_controls { 6 } else { 3 }),
        Constraint::Length(5),
    ])
    .split(area);
    let phase_color = match &state.phase {
        AgentPhase::Idle => Color::Gray,
        AgentPhase::PreparingReview => Color::LightMagenta,
        AgentPhase::Planning => Color::LightBlue,
        AgentPhase::AwaitingPlanApproval => Color::Magenta,
        AgentPhase::Requesting => Color::Blue,
        AgentPhase::Streaming => Color::Green,
        AgentPhase::Parsing => Color::Cyan,
        AgentPhase::ExecutingTools => Color::Yellow,
        AgentPhase::AwaitingPatchApproval
        | AgentPhase::AwaitingConfirmation
        | AgentPhase::AwaitingContinuation => Color::Magenta,
        AgentPhase::Error { .. } => Color::Red,
    };
    let now = Instant::now();
    let elapsed = now.saturating_duration_since(state.phase_started);
    let throbber = Throbber::default()
        .style(Style::default().fg(phase_color))
        .throbber_style(
            Style::default()
                .fg(phase_color)
                .add_modifier(Modifier::BOLD),
        )
        .throbber_set(BRAILLE_SIX)
        .use_type(WhichUse::Spin);
    let mut throbber_state = ThrobberState::default();
    throbber_state.calc_step(((elapsed.as_millis() / 125) % 120) as i8);
    let phase = phase_label(&state.phase);
    let last_whip = match state.whip.last_acknowledgement {
        Some(WhipDisplayKind::Soft) => text(Text::Low),
        Some(WhipDisplayKind::Hard) => text(Text::High),
        None => "-",
    };
    let connection = if state.connected {
        connection_status_label(&state.connection_status)
    } else {
        text(Text::ClosedStatus).to_owned()
    };
    let eta = state.eta.estimate(
        &state.phase,
        now,
        state.running_tools.len(),
        state
            .work_modes
            .goal
            .as_ref()
            .map(|goal| goal.next_steps.len()),
    );
    let eta_line = eta.map_or_else(
        || format!("{}: —", text(Text::Estimate)),
        |estimate| {
            let mut label = format!(
                "{}: {}–{} ({}%)",
                text(Text::Estimate),
                format_duration(estimate.low),
                format_duration(estimate.high),
                estimate.confidence_percent
            );
            if let Some(backtest) = state.eta.backtest()
                && backtest.samples >= 3
            {
                label.push_str(&format!(
                    " · MAE {} · MdAE {} · {}%",
                    format_duration(backtest.mean_absolute_error),
                    format_duration(backtest.median_absolute_error),
                    backtest.interval_coverage_percent
                ));
            }
            label
        },
    );
    let turn_elapsed = state.eta.turn_elapsed(now).unwrap_or(Duration::ZERO);
    let cost_line = format!("{}: {}", text(Text::Cost), usage_cost_label(&state.usage));
    let live_tokens = approximate_live_tokens(state);
    let token_line = if state.phase.is_busy() && live_tokens > 0 {
        format!(
            "{}: {}/{}/{} · +~{}",
            text(Text::Tokens),
            compact_token_count(state.tokens_input),
            compact_token_count(state.tokens_output),
            compact_token_count(state.tokens_total),
            compact_token_count(live_tokens)
        )
    } else {
        format!(
            "{}: {}/{}/{}",
            text(Text::Tokens),
            compact_token_count(state.tokens_input),
            compact_token_count(state.tokens_output),
            compact_token_count(state.tokens_total)
        )
    };
    let last_context = state
        .usage
        .last_response_tokens
        .map_or_else(|| "—".to_owned(), |tokens| tokens.to_string());
    let mut lines = vec![
        Line::from(vec![
            throbber.to_symbol_span(&throbber_state),
            Span::styled(
                format!(
                    " {phase} · {} {:.1}s",
                    text(Text::SegmentLabel),
                    elapsed.as_secs_f32()
                ),
                Style::default().fg(phase_color),
            ),
        ]),
        Line::from(format!(
            "{}: {} · {} {}",
            text(Text::Elapsed),
            format_duration(turn_elapsed),
            text(Text::SessionLower),
            format_duration(state.eta.session_elapsed(now))
        )),
        Line::from(eta_line),
        Line::from(token_line),
        Line::from(cost_line),
        Line::from(format!(
            "{}: {}",
            text(Text::Model),
            compact_single_line(&state.deployment, 22)
        )),
        Line::from(format!(
            "{}: {}/{} · {}",
            text(Text::Context),
            compact_token_count(u64::from(state.context_budget)),
            last_context
                .parse::<u64>()
                .map(compact_token_count)
                .unwrap_or(last_context),
            context_mode_label(state.context_mode)
        )),
        Line::from(format!("{}: {connection}", text(Text::Connection))),
        Line::from(format!(
            "{}: {}",
            text(Text::Turn),
            state
                .active_turn_id
                .map_or_else(|| "-".into(), |id| id.to_string())
        )),
        Line::from(format!(
            "{}: {}/{}",
            text(Text::EpochRevision),
            state.conversation_epoch,
            state.phase_revision
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::ContextCeiling),
            state.max_context_budget
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::ReasoningBase),
            reasoning_effort_label(state.reasoning_effort)
        )),
        Line::from(format!(
            "{}: P:{} E:{} R:{} G:{} D:{}",
            text(Text::Modes),
            on_off(state.work_modes.plan),
            on_off(state.work_modes.explore),
            on_off(state.work_modes.review),
            on_off(state.work_modes.goal_enabled()),
            on_off(state.work_modes.deep_thinking)
        )),
        Line::from(format!(
            "{}: {}/{}",
            text(Text::Terminals),
            state
                .terminal
                .sessions
                .iter()
                .filter(|session| session.status.is_active())
                .count(),
            state.terminal.sessions.len()
        )),
        Line::from(format!(
            "LSP: {}/{} {} · {} {}",
            state
                .lsp_servers
                .iter()
                .filter(|server| server.state == crate::lsp::LspConnectionState::Connected)
                .count(),
            state.lsp_servers.len(),
            text(Text::Ready),
            state.lsp_diagnostics.len(),
            text(Text::DiagnosticsLabel)
        )),
        Line::from(format!(
            "{}: {} · {} {} / {} {}",
            text(Text::Index),
            code_index_state_label(state.code_index.state),
            state.code_index.indexed_files,
            text(Text::FilesLabel),
            state.code_index.chunk_count,
            text(Text::ChunksLabel)
        )),
        Line::from(format!(
            "{}: {}/{} · {} B",
            text(Text::Instructions),
            state
                .instructions
                .sources
                .iter()
                .filter(|source| {
                    source.locked || (state.instructions.project_enabled && source.enabled)
                })
                .count(),
            state.instructions.sources.len(),
            state.instructions.active_project_bytes
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::UsageTotal),
            state.tokens_total
        )),
        Line::from(format!(
            "{}: {}/{}",
            text(Text::TokensInOut),
            state.tokens_input,
            state.tokens_output
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::EstimatedCost),
            usage_cost_label(&state.usage)
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::SideQuestions),
            state.side_chat.exchanges.len()
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::QueueSteer),
            state.follow_ups.pending_count()
        )),
        Line::from(format!(
            "{}: {} · {}",
            text(Text::Reviews),
            state.reviews.reports.len(),
            state.reviews.open_findings()
        )),
        Line::from(""),
        Line::from(format!(
            "{}: {}",
            text(Text::WhipStrikes),
            state.whip_telemetry.total_strikes
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::PenaltyLeft),
            state.whip_telemetry.penalty_responses_remaining
        )),
        Line::from(format!(
            "{}: {}",
            text(Text::BudgetSaved),
            state.whip_telemetry.estimated_saved_token_budget
        )),
        Line::from(format!(
            "{}: {}/{} ({last_whip})",
            text(Text::UiSentAcknowledged),
            state.whip.requests_sent,
            state.whip.acknowledgements
        )),
    ];
    if let Some(retry) = &state.retry {
        lines.push(Line::from(Span::styled(
            format!(
                "{}: {}/{}",
                text(Text::Retry),
                retry.next_attempt,
                retry.max_attempts
            ),
            Style::default().fg(Color::Yellow),
        )));
        lines.push(Line::from(format!(
            "{}: {}",
            text(Text::ReasonLabel),
            compact_single_line(&retry.reason, 48)
        )));
    }
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", text(Text::Status))),
        ),
        side_chunks[0],
    );

    let can_whip = state.can_whip();
    let animation = [
        format!("[{}]  WHIP", state.whip_hotkey),
        format!("[{}] >>WHIP", state.whip_hotkey),
        format!("[{}] >>>WHIP", state.whip_hotkey),
    ];
    let animation = animation[state.whip.frame(now)].as_str();
    let whip_style = if state.whip.is_flashing(now) {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if can_whip {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let utility = if state.mascot.enabled() {
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(side_chunks[1])
    } else {
        Layout::horizontal([Constraint::Length(0), Constraint::Fill(1)]).split(side_chunks[1])
    };
    if state.mascot.enabled() {
        let mood = state.mascot.mood(&state.phase, now);
        let available = usize::from(utility[0].width.saturating_sub(2)).max(1);
        let label = truncate_for_display(
            &format!("Pixel {}", state.mascot.mini_face(mood)),
            available,
        );
        let pet_state = if state.phase.is_busy() {
            ButtonState::disabled()
        } else {
            ButtonState::enabled()
        };
        let pet_button = Button::new(&label, &pet_state)
            .variant(ButtonVariant::Block)
            .style(match mood {
                MascotMood::Working => ButtonStyle::primary(),
                MascotMood::Celebrating
                | MascotMood::VictoryRolling
                | MascotMood::Playful
                | MascotMood::Pouncing
                | MascotMood::Affectionate
                | MascotMood::Purring
                | MascotMood::Stretching
                | MascotMood::Dancing => ButtonStyle::success(),
                MascotMood::Error => ButtonStyle::danger(),
                MascotMood::Hungry | MascotMood::Overfed => ButtonStyle::primary(),
                MascotMood::Waiting
                | MascotMood::Curious
                | MascotMood::Blinking
                | MascotMood::Waving
                | MascotMood::Grooming
                | MascotMood::Rolling
                | MascotMood::Chasing
                | MascotMood::Stargazing
                | MascotMood::Tongue
                | MascotMood::Yawning
                | MascotMood::Sleeping
                | MascotMood::Burping => ButtonStyle::default(),
            });
        let region = pet_button.render_stateful(utility[0], frame.buffer_mut());
        if !state.phase.is_busy() {
            state.shell_ui.register_hit(
                region.area,
                if mood == MascotMood::Sleeping {
                    ShellHit::MascotWake
                } else {
                    ShellHit::MascotFeed
                },
            );
        }
    }
    frame.render_widget(
        Paragraph::new(if can_whip {
            animation.to_owned()
        } else {
            format!("Whip {}", text(Text::Unavailable))
        })
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL))
        .style(whip_style),
        utility[1],
    );
    state.whip_hitbox = Some(utility[1]);
    let sessions_enabled = matches!(state.phase, AgentPhase::Idle | AgentPhase::Error { .. });
    let navigation = if expanded_rule_controls {
        let columns = Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)])
            .split(side_chunks[2]);
        [columns[0], columns[1]]
    } else {
        [side_chunks[2], side_chunks[3]]
    };
    state
        .session_ui
        .draw_launcher(frame, navigation[0], sessions_enabled, state.sessions.len());
    let rewind_enabled = matches!(state.phase, AgentPhase::Idle) && !state.checkpoints.is_empty();
    state.rewind_ui.draw_launcher(
        frame,
        navigation[1],
        rewind_enabled,
        state.checkpoints.len(),
    );
    let runtime_enabled = matches!(
        state.phase,
        AgentPhase::Idle
            | AgentPhase::Error {
                recoverable: true,
                ..
            }
    );
    let mut runtime_state = if runtime_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    runtime_state.set_focused(false);
    let (effective_effort, reasoning_mode) =
        state.work_modes.effective_reasoning(state.reasoning_effort);
    let profile = if reasoning_mode.is_some() {
        "Ultra".to_owned()
    } else {
        effective_effort.to_string()
    };
    let runtime_label = format!(
        "{} · {profile} · {}",
        text(Text::Runtime),
        compact_context_budget(state.context_budget)
    );
    let runtime_region = Button::new(&runtime_label, &runtime_state)
        .variant(ButtonVariant::Block)
        .style(if reasoning_mode.is_some() {
            ButtonStyle::primary()
        } else {
            ButtonStyle::default()
        })
        .render_stateful(side_chunks[4], frame.buffer_mut());
    if runtime_enabled {
        state
            .shell_ui
            .register_hit(runtime_region.area, ShellHit::RuntimeManager);
    }
    let modes_enabled = matches!(state.phase, AgentPhase::Idle);
    let active_modes = usize::from(state.work_modes.plan)
        + usize::from(state.work_modes.explore)
        + usize::from(state.work_modes.review)
        + usize::from(state.work_modes.goal_enabled())
        + usize::from(state.work_modes.deep_thinking);
    let mut modes_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    modes_state.set_focused(false);
    let modes_label = format!("{} · {active_modes}/5", text(Text::WorkModes));
    let modes_button = Button::new(&modes_label, &modes_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let modes_region = modes_button.render_stateful(side_chunks[5], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(modes_region.area, ShellHit::ModesManager);
    }
    let mut follow_state = ButtonState::enabled();
    follow_state.set_focused(false);
    let follow_label = format!(
        "{} · {}",
        text(Text::QueueSteer),
        state.follow_ups.pending_count()
    );
    let follow_button = Button::new(&follow_label, &follow_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let follow_region = follow_button.render_stateful(side_chunks[6], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(follow_region.area, ShellHit::FollowUps);
    let mut side_state = ButtonState::enabled();
    side_state.set_focused(false);
    let side_label = match state.side_chat.latest().map(|exchange| exchange.status) {
        Some(crate::agent::SideExchangeStatus::Running) => {
            format!("/btw · {}", text(Text::ThinkingEllipsis))
        }
        Some(crate::agent::SideExchangeStatus::Completed) => {
            format!("/btw · {}", text(Text::AnswerReady))
        }
        Some(crate::agent::SideExchangeStatus::Failed) => {
            format!("/btw · {}", text(Text::FailedStatus))
        }
        Some(crate::agent::SideExchangeStatus::Cancelled) => {
            format!("/btw · {}", text(Text::Cancelled))
        }
        None => format!("/btw · {}", text(Text::AskAside)),
    };
    let side_button = Button::new(&side_label, &side_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let side_region = side_button.render_stateful(side_chunks[7], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(side_region.area, ShellHit::SideChat);
    let mut usage_state = ButtonState::enabled();
    usage_state.set_focused(false);
    let usage_label = match state.usage.cost_coverage() {
        CostCoverage::Complete => {
            format!("{} · {}", text(Text::Usage), usage_cost_label(&state.usage))
        }
        CostCoverage::Partial => format!(
            "{} · {}+?",
            text(Text::Usage),
            format_microusd(state.usage.estimated_cost_microusd)
        ),
        CostCoverage::NoUsage | CostCoverage::Unpriced => {
            format!(
                "{} · {} {}",
                text(Text::Usage),
                state.tokens_total,
                text(Text::Tokens)
            )
        }
    };
    let usage_button = Button::new(&usage_label, &usage_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let usage_region = usage_button.render_stateful(side_chunks[8], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(usage_region.area, ShellHit::UsageManager);
    let inbox_review = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(side_chunks[9]);
    let mut inbox_state = ButtonState::enabled();
    inbox_state.set_focused(false);
    let inbox_label = if state.notifications.unread_count() == 0 {
        format!("{} · 0", text(Text::Inbox))
    } else {
        format!(
            "{} · ●{}",
            text(Text::Inbox),
            state.notifications.unread_count()
        )
    };
    let inbox_button = Button::new(&inbox_label, &inbox_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let inbox_region = inbox_button.render_stateful(inbox_review[0], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(inbox_region.area, ShellHit::NotificationCenter);
    let mut review_state = ButtonState::enabled();
    review_state.set_focused(false);
    let review_label = format!(
        "{} · {}",
        text(Text::Reviews),
        state.reviews.open_findings()
    );
    let review_button = Button::new(&review_label, &review_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let review_region = review_button.render_stateful(inbox_review[1], frame.buffer_mut());
    state
        .shell_ui
        .register_hit(review_region.area, ShellHit::ReviewManager);
    let instruction_sources = state.instructions.sources.len().saturating_sub(1);
    let instruction_warnings = state.instructions.warnings.len();
    let mut instructions_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    instructions_state.set_focused(false);
    let instructions_label = format!(
        "{} {instruction_sources}{}",
        text(Text::Rules),
        if instruction_warnings == 0 {
            String::new()
        } else {
            format!(" · ⚠{instruction_warnings}")
        }
    );
    let rule_controls = if expanded_rule_controls {
        let rows =
            Layout::vertical([Constraint::Length(3), Constraint::Length(3)]).split(side_chunks[10]);
        let top = Layout::horizontal([
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
            Constraint::Ratio(1, 3),
        ])
        .split(rows[0]);
        let bottom =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).split(rows[1]);
        [top[0], top[1], top[2], bottom[0], bottom[1]]
    } else {
        let columns = Layout::horizontal([
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
            Constraint::Percentage(20),
        ])
        .split(side_chunks[10]);
        [columns[0], columns[1], columns[2], columns[3], columns[4]]
    };
    let instructions_label = fit_button_label(&instructions_label, rule_controls[0]);
    let instructions_button = Button::new(&instructions_label, &instructions_state)
        .variant(ButtonVariant::Block)
        .style(ButtonStyle::default());
    let instructions_region =
        instructions_button.render_stateful(rule_controls[0], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(instructions_region.area, ShellHit::InstructionsManager);
    }
    let enabled_skills = state
        .skills
        .skills
        .iter()
        .filter(|skill| skill.enabled)
        .count();
    let mut skills_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    skills_state.set_focused(false);
    let skills_label = format!(
        "{} {enabled_skills}/{}",
        text(Text::Skills),
        state.skills.skills.len()
    );
    let skills_label = fit_button_label(&skills_label, rule_controls[1]);
    let skills_region = Button::new(&skills_label, &skills_state)
        .variant(ButtonVariant::Block)
        .style(if state.skills.metadata_omitted == 0 {
            ButtonStyle::default()
        } else {
            ButtonStyle::primary()
        })
        .render_stateful(rule_controls[1], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(skills_region.area, ShellHit::SkillsManager);
    }
    let privacy_failed = state
        .privacy
        .sources
        .iter()
        .any(|source| source.fail_closed);
    let privacy_label = if privacy_failed {
        format!("{} !", text(Text::Shield))
    } else {
        format!("{} {}", text(Text::Shield), state.privacy.blocked_attempts)
    };
    let mut privacy_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    privacy_state.set_focused(false);
    let privacy_label = fit_button_label(&privacy_label, rule_controls[2]);
    let privacy_region = Button::new(&privacy_label, &privacy_state)
        .variant(ButtonVariant::Block)
        .style(if privacy_failed {
            ButtonStyle::danger()
        } else {
            ButtonStyle::default()
        })
        .render_stateful(rule_controls[2], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(privacy_region.area, ShellHit::PrivacyShield);
    }
    let permission_count = state.shell_permissions.grants.len();
    let mut permission_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    permission_state.set_focused(false);
    let permission_label = format!("{} {permission_count}", text(Text::Permissions));
    let permission_label = fit_button_label(&permission_label, rule_controls[3]);
    let permission_region = Button::new(&permission_label, &permission_state)
        .variant(ButtonVariant::Block)
        .style(if permission_count == 0 {
            ButtonStyle::default()
        } else {
            ButtonStyle::primary()
        })
        .render_stateful(rule_controls[3], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(permission_region.area, ShellHit::ShellPermissions);
    }
    let mut approval_state = if modes_enabled {
        ButtonState::enabled()
    } else {
        ButtonState::disabled()
    };
    approval_state.set_focused(false);
    let approval_label = format!(
        "{} {}/8",
        text(Text::AutoApproval),
        state.auto_approval.enabled_count()
    );
    let approval_label = fit_button_label(&approval_label, rule_controls[4]);
    let approval_region = Button::new(&approval_label, &approval_state)
        .variant(ButtonVariant::Block)
        .style(if state.auto_approval.enabled_count() == 0 {
            ButtonStyle::default()
        } else {
            ButtonStyle::primary()
        })
        .render_stateful(rule_controls[4], frame.buffer_mut());
    if modes_enabled {
        state
            .shell_ui
            .register_hit(approval_region.area, ShellHit::AutoApprovalCenter);
    }
    if state.phase.is_error() {
        let error_chunks =
            Layout::vertical([Constraint::Length(3), Constraint::Length(2)]).split(side_chunks[11]);
        let buttons = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(error_chunks[0]);
        let retry_focused = state.shell_ui.retry_is_focused();
        let recoverable = matches!(
            state.phase,
            AgentPhase::Error {
                recoverable: true,
                ..
            }
        );
        let mut retry_state = if recoverable {
            ButtonState::enabled()
        } else {
            ButtonState::disabled()
        };
        retry_state.set_focused(retry_focused);
        let retry = Button::new(text(Text::Retry), &retry_state)
            .icon("↻")
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::primary());
        let retry_region = retry.render_stateful(buttons[0], frame.buffer_mut());
        if recoverable {
            state
                .shell_ui
                .register_hit(retry_region.area, ShellHit::RetryFailedTurn);
        }

        let mut abort_state = ButtonState::enabled();
        abort_state.set_focused(!retry_focused);
        let abort = Button::new(text(Text::Abort), &abort_state)
            .icon("×")
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::danger());
        let abort_region = abort.render_stateful(buttons[1], frame.buffer_mut());
        state
            .shell_ui
            .register_hit(abort_region.area, ShellHit::AbortFailedTurn);
        frame.render_widget(
            Paragraph::new(text(Text::TabHint))
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            error_chunks[1],
        );
        return;
    }
    if state.phase.is_busy() || state.paused_turn_id.is_some() {
        let controls =
            Layout::vertical([Constraint::Length(3), Constraint::Length(2)]).split(side_chunks[11]);
        let paused = state.paused_turn_id.is_some() && !state.phase.is_busy();
        if paused {
            let buttons =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(controls[0]);
            let control_state = ButtonState::enabled();
            let resume_label = format!("> {} (F6)", text(Text::Continue));
            let resume = Button::new(&resume_label, &control_state)
                .variant(ButtonVariant::Block)
                .style(ButtonStyle::success());
            let resume_region = resume.render_stateful(buttons[0], frame.buffer_mut());
            state
                .shell_ui
                .register_hit(resume_region.area, ShellHit::ResumePausedTurn);

            let abort_label = format!("× {} (F8)", text(Text::Abort));
            let abort = Button::new(&abort_label, &control_state)
                .variant(ButtonVariant::Block)
                .style(ButtonStyle::danger());
            let abort_region = abort.render_stateful(buttons[1], frame.buffer_mut());
            state
                .shell_ui
                .register_hit(abort_region.area, ShellHit::AbortPausedTurn);
        } else {
            let label = format!("|| {} (F6)", text(Text::Pause));
            let control_state = ButtonState::enabled();
            let button = Button::new(&label, &control_state)
                .variant(ButtonVariant::Block)
                .style(ButtonStyle::primary());
            let region = button.render_stateful(controls[0], frame.buffer_mut());
            state
                .shell_ui
                .register_hit(region.area, ShellHit::PauseTurn);
        }
        frame.render_widget(
            Paragraph::new(if paused {
                text(Text::DurableReplayExplicit)
            } else {
                text(Text::CancelPersistResume)
            })
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(Color::DarkGray)),
            controls[1],
        );
        return;
    }
    let help = {
        vec![
            Line::from(text(Text::EnterSubmitHelp)),
            Line::from(text(Text::InterruptHelp)),
            Line::from(text(Text::ExpandToolHelp)),
        ]
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        side_chunks[11],
    );
}

fn approximate_live_tokens(state: &AppState) -> u64 {
    [
        &state.live_thinking,
        &state.live_assistant,
        &state.interrupted_draft,
    ]
    .into_iter()
    .map(|value| {
        let characters = u64::try_from(value.chars().count()).unwrap_or(u64::MAX);
        characters.div_ceil(4)
    })
    .fold(0_u64, u64::saturating_add)
}

fn draw_input(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if state.phase.is_busy() {
            format!(" {} | {} ", text(Text::Input), text(Text::BusyNoQueue))
        } else {
            format!(" {} | {} ", text(Text::Input), text(Text::EnterAttachHelp))
        });
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let content = if state.pending_attachments.is_empty() {
        vec![inner]
    } else {
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)])
            .split(inner)
            .to_vec()
    };
    let text_area = *content.last().unwrap_or(&inner);
    if let Some(chips) = content.first().filter(|_| content.len() > 1) {
        draw_attachment_chips(frame, *chips, state);
    }
    let display = if state.input_buffer.is_empty() {
        if state.pending_attachments.is_empty() {
            text(Text::PromptHint).to_owned()
        } else {
            text(Text::AttachmentHint).to_owned()
        }
    } else {
        sanitize_for_display(&state.input_buffer)
    };
    let mut cursor = state.input_cursor.min(state.input_buffer.len());
    while !state.input_buffer.is_char_boundary(cursor) {
        cursor = cursor.saturating_sub(1);
    }
    let prefix = sanitize_for_display(&state.input_buffer[..cursor]);
    let mut cursor_row = 0usize;
    let mut cursor_column = 0usize;
    let width = usize::from(text_area.width).max(1);
    for segment in prefix.split_inclusive('\n') {
        let has_newline = segment.ends_with('\n');
        let segment = segment.trim_end_matches('\n');
        let cells = UnicodeWidthStr::width(segment);
        cursor_row += (cursor_column + cells) / width;
        cursor_column = (cursor_column + cells) % width;
        if has_newline {
            cursor_row += 1;
            cursor_column = 0;
        }
    }
    let vertical_scroll =
        cursor_row.saturating_sub(usize::from(text_area.height.saturating_sub(1)));
    let paragraph = if state.input_buffer.is_empty() {
        Paragraph::new(Span::styled(display, Style::default().fg(Color::DarkGray)))
    } else {
        Paragraph::new(display)
    };
    frame.render_widget(
        paragraph
            .wrap(Wrap { trim: false })
            .scroll((vertical_scroll.min(usize::from(u16::MAX)) as u16, 0)),
        text_area,
    );

    if !state.has_blocking_modal() && text_area.width > 0 && text_area.height > 0 {
        let visible_row = cursor_row.saturating_sub(vertical_scroll);
        frame.set_cursor_position(Position::new(
            text_area
                .x
                .saturating_add(cursor_column.min(width - 1) as u16),
            text_area.y.saturating_add(visible_row as u16),
        ));
    }
}

fn draw_attachment_chips(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    let mut x = area.x;
    for (index, attachment) in state.pending_attachments.iter().enumerate() {
        if x >= area.right() {
            break;
        }
        let label = format!(
            " {} {} × ",
            attachment.kind.label(),
            compact_single_line(&attachment.filename, 22)
        );
        let width = u16::try_from(UnicodeWidthStr::width(label.as_str()))
            .unwrap_or(u16::MAX)
            .min(area.right().saturating_sub(x));
        if width < 4 {
            break;
        }
        let chip_area = Rect::new(x, area.y, width, 1);
        let chip_state = ButtonState::enabled();
        let chip = Button::new(&label, &chip_state)
            .variant(ButtonVariant::Block)
            .style(ButtonStyle::primary());
        let region = chip.render_stateful(chip_area, frame.buffer_mut());
        state
            .shell_ui
            .register_hit(region.area, ShellHit::RemoveAttachment(index));
        x = x.saturating_add(width).saturating_add(1);
    }
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    let status = sanitize_for_display(
        state
            .status_message
            .as_deref()
            .unwrap_or_else(|| text(Text::Ready)),
    );
    let now = Instant::now();
    let timing = if state.phase.is_busy() {
        let elapsed = state
            .eta
            .turn_elapsed(now)
            .unwrap_or_else(|| state.phase_started.elapsed());
        state
            .eta
            .estimate(
                &state.phase,
                now,
                state.running_tools.len(),
                state
                    .work_modes
                    .goal
                    .as_ref()
                    .map(|goal| goal.next_steps.len()),
            )
            .map_or_else(
                || format!("{} {}", text(Text::Elapsed), format_duration(elapsed)),
                |estimate| {
                    format!(
                        "{} {} · {} {}–{} ({}%)",
                        text(Text::Elapsed),
                        format_duration(elapsed),
                        text(Text::Estimate),
                        format_duration(estimate.low),
                        format_duration(estimate.high),
                        estimate.confidence_percent
                    )
                },
            )
    } else {
        format!(
            "{} {} · {} {} · {} {}",
            text(Text::SessionLower),
            format_duration(state.eta.session_elapsed(now)),
            text(Text::Tokens),
            state.tokens_total,
            text(Text::Cost),
            usage_cost_label(&state.usage)
        )
    };
    let style = if state.phase.is_error() {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::White)
    };
    let chunks = Layout::horizontal([
        Constraint::Min(10),
        Constraint::Length(48.min(area.width / 2)),
    ])
    .split(area);
    frame.render_widget(Paragraph::new(Span::styled(status, style)), chunks[0]);
    frame.render_widget(
        Paragraph::new(timing)
            .alignment(Alignment::Right)
            .style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

fn usage_cost_label(usage: &UsageSnapshot) -> String {
    match usage.cost_coverage() {
        CostCoverage::NoUsage => "—".to_owned(),
        CostCoverage::Unpriced => text(Text::Unpriced).to_owned(),
        CostCoverage::Partial => format!(
            "{} + {}",
            format_microusd(usage.estimated_cost_microusd),
            text(Text::Unpriced)
        ),
        CostCoverage::Complete => format_microusd(usage.estimated_cost_microusd),
    }
}

fn compact_context_budget(tokens: u32) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens >= 1_000_000 && tokens.is_multiple_of(1_000_000) {
        format!("{}M", tokens / 1_000_000)
    } else {
        format!("{}K", tokens / 1_000)
    }
}

fn compact_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 100_000 {
        format!("{}K", (tokens + 500) / 1_000)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn format_duration(duration: std::time::Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds < 60 {
        return format!("{:.1}s", duration.as_secs_f64());
    }
    let hours = total_seconds / 3_600;
    let minutes = (total_seconds % 3_600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[must_use]
pub fn sanitize_for_display(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    use std::fmt::Write as _;
    for character in value.chars() {
        match character {
            '\n' => output.push('\n'),
            '\t' => output.push_str("\\t"),
            '\r' => output.push_str("\\r"),
            '\u{08}' => output.push_str("\\b"),
            '\u{1b}' => output.push_str("\\x1b"),
            character if is_bidi_control(character) => {
                let _ = write!(output, "<U+{:04X}>", character as u32);
            }
            character if character.is_control() => {
                let code = character as u32;
                if code <= 0xff {
                    let _ = write!(output, "\\x{code:02x}");
                } else {
                    let _ = write!(output, "\\u{{{code:x}}}");
                }
            }
            character => output.push(character),
        }
    }
    output
}

fn is_bidi_control(character: char) -> bool {
    matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

#[must_use]
pub fn truncate_for_display(value: &str, max_graphemes: usize) -> String {
    let count = value.graphemes(true).count();
    if count <= max_graphemes {
        return value.to_owned();
    }
    if max_graphemes == 0 {
        return String::new();
    }
    if max_graphemes == 1 {
        return "…".to_owned();
    }
    let mut truncated = value
        .graphemes(true)
        .take(max_graphemes - 1)
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[must_use]
pub fn strip_service_blocks(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;

    while cursor < value.len() {
        let Some(relative_tag_start) = value[cursor..].find('<') else {
            output.push_str(&value[cursor..]);
            break;
        };

        let tag_start = cursor.saturating_add(relative_tag_start);
        output.push_str(&value[cursor..tag_start]);
        let tag_suffix = &value[tag_start..];
        let Some((opening, closing)) = SERVICE_BLOCKS
            .iter()
            .copied()
            .find(|(opening, _)| tag_suffix.starts_with(opening))
        else {
            // Unknown tags and ordinary less-than signs are user-visible text.
            // Advancing past this one ASCII byte keeps the scan strictly linear.
            output.push('<');
            cursor = tag_start.saturating_add(1);
            continue;
        };

        let content_start = tag_start.saturating_add(opening.len());
        let Some(block_end) = service_block_end(value, content_start, opening, closing) else {
            // A canonical service block without its exact closing tag may contain
            // private reasoning or tool arguments. Drop the entire uncertain tail.
            break;
        };
        cursor = block_end;
    }
    output.trim().to_owned()
}

fn service_block_end(
    value: &str,
    content_start: usize,
    opening: &str,
    closing: &str,
) -> Option<usize> {
    let mut cursor = content_start;
    let mut depth = 1_usize;
    while cursor < value.len() {
        let next_open = value[cursor..]
            .find(opening)
            .map(|offset| cursor.saturating_add(offset));
        let next_close = value[cursor..]
            .find(closing)
            .map(|offset| cursor.saturating_add(offset));
        match (next_open, next_close) {
            (None, Some(close)) => {
                depth = depth.saturating_sub(1);
                cursor = close.saturating_add(closing.len());
                if depth == 0 {
                    return Some(cursor);
                }
            }
            (Some(open), Some(close)) if close < open => {
                depth = depth.saturating_sub(1);
                cursor = close.saturating_add(closing.len());
                if depth == 0 {
                    return Some(cursor);
                }
            }
            (Some(open), _) => {
                depth = depth.saturating_add(1);
                cursor = open.saturating_add(opening.len());
            }
            (None, None) => return None,
        }
    }
    None
}

fn extract_thinking_blocks(value: &str) -> Vec<&str> {
    const OPEN: &str = "<thinking>";
    const CLOSE: &str = "</thinking>";
    let mut result = Vec::new();
    let mut cursor = 0;
    while cursor < value.len() {
        let Some(relative_start) = value[cursor..].find(OPEN) else {
            break;
        };
        let start = cursor
            .saturating_add(relative_start)
            .saturating_add(OPEN.len());
        let Some(relative_end) = value[start..].find(CLOSE) else {
            break;
        };
        let end = start.saturating_add(relative_end);
        result.push(value[start..end].trim());
        cursor = end.saturating_add(CLOSE.len());
    }
    result
}

fn compact_single_line(value: &str, max_graphemes: usize) -> String {
    let safe = sanitize_for_display(value);
    let flattened = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_for_display(&flattened, max_graphemes)
}

fn button_content_width(label: &str) -> u16 {
    u16::try_from(UnicodeWidthStr::width(label))
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

fn fit_button_label(label: &str, area: Rect) -> String {
    truncate_for_display(label, usize::from(area.width.saturating_sub(2)))
}

#[cfg(test)]
mod tests {
    use super::{
        D_MORPH_FRAMES, animated_d_frame, assistant_speaker, compact_context_budget,
        extract_thinking_blocks, sanitize_for_display, shimmer_line, strip_service_blocks,
        truncate_for_display, usage_cost_label,
    };
    use crate::usage::{DeploymentPricing, PricingCatalog, UsageLedger};
    use ratatui::style::Color;
    use std::time::Duration;

    #[test]
    fn unicode_truncation_never_slices_a_grapheme() {
        assert_eq!(truncate_for_display("a👩‍💻b", 2), "a…");
        assert_eq!(truncate_for_display("hello", 5), "hello");
        assert_eq!(truncate_for_display("hello", 0), "");
    }

    #[test]
    fn sub_thousand_context_budget_is_not_rounded_down_to_zero() {
        assert_eq!(compact_context_budget(999), "999");
    }

    #[test]
    fn display_text_escapes_terminal_and_bidi_controls() {
        let safe = sanitize_for_display("ok\u{1b}[2J\u{202e}bad\r");
        assert_eq!(safe, "ok\\x1b[2J<U+202E>bad\\r");
    }

    #[test]
    fn activity_shimmer_preserves_text_and_moves_its_highlight() {
        let first = shimmer_line("working", Duration::ZERO, Color::DarkGray);
        let later = shimmer_line("working", Duration::from_millis(225), Color::DarkGray);
        let first_text = first
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let later_text = later
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(first_text, "working");
        assert_eq!(later_text, "working");
        assert_ne!(first.spans[0].style, later.spans[0].style);
    }

    #[test]
    fn d_pulse_uses_only_clear_scale_steps_and_returns_smoothly() {
        use std::collections::BTreeSet;

        use unicode_width::UnicodeWidthStr;

        assert_eq!(D_MORPH_FRAMES, ["ᴅ", "ᴅ", "D", "𝐃", "𝐃", "D", "ᴅ", "ᴅ"]);
        let distinct = D_MORPH_FRAMES.into_iter().collect::<BTreeSet<_>>();
        assert_eq!(distinct.len(), 3);
        for (index, frame) in D_MORPH_FRAMES.into_iter().enumerate() {
            assert_eq!(UnicodeWidthStr::width(frame), 1, "frame {index}: {frame:?}");
            assert_eq!(animated_d_frame(index), frame);
        }
        assert_eq!(animated_d_frame(D_MORPH_FRAMES.len()), D_MORPH_FRAMES[0]);
    }

    #[test]
    fn assistant_speaker_is_stable_with_an_occasional_pixel_alias() {
        assert_eq!(assistant_speaker(3, 17), assistant_speaker(3, 17));
        let pixel_count = (1..=100)
            .filter(|turn_id| assistant_speaker(3, *turn_id) == "Pixel")
            .count();
        assert!((12..=28).contains(&pixel_count));
    }

    #[test]
    fn cost_label_never_formats_unknown_deployment_as_zero_dollars()
    -> Result<(), Box<dyn std::error::Error>> {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "placeholder".to_owned(),
            1.0,
            None,
            2.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("gpt-5.6-sol", 4_793, 0, 118, 4_911);
        let snapshot = catalog.snapshot(&ledger, Some(4_911));

        assert_eq!(usage_cost_label(&snapshot), "unpriced");
        Ok(())
    }

    #[test]
    fn final_text_drops_thinking_and_tool_blocks() {
        let value = concat!(
            "Before\n",
            "<thinking>secret</thinking>\n",
            "<read_file><path>x</path></read_file>\n",
            "After"
        );
        assert_eq!(strip_service_blocks(value), "Before\n\n\nAfter");
    }

    #[test]
    fn model_emitted_thinking_can_be_rendered_separately_without_exposing_unclosed_tails() {
        let value = "<thinking>first</thinking>answer<thinking>second</thinking>";
        assert_eq!(extract_thinking_blocks(value), vec!["first", "second"]);
        assert!(extract_thinking_blocks("<thinking>unfinished secret").is_empty());
    }

    #[test]
    fn unclosed_service_block_is_not_exposed() {
        assert_eq!(
            strip_service_blocks("Visible<thinking>unfinished"),
            "Visible"
        );
    }

    #[test]
    fn nested_service_blocks_do_not_expose_the_outer_private_tail() {
        assert_eq!(
            strip_service_blocks(
                "Visible<thinking>outer<thinking>inner</thinking>still private</thinking>Done"
            ),
            "VisibleDone"
        );
    }

    #[test]
    fn many_same_kind_blocks_are_stripped_in_one_forward_scan() {
        let mut value = String::new();
        let mut expected = String::new();
        for _ in 0..4_096 {
            value.push_str("visible<unknown>kept</unknown>");
            value.push_str("<thinking>private</thinking>");
            expected.push_str("visible<unknown>kept</unknown>");
        }

        assert_eq!(strip_service_blocks(&value), expected);
    }

    #[test]
    fn unknown_and_stray_closing_tags_remain_plain_text() {
        let value = "a<custom>body</custom></thinking>b";
        assert_eq!(strip_service_blocks(value), value);
    }
}
