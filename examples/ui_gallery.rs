//! Offline visual QA harness. It renders the real TUI into a deterministic
//! TestBackend, so layout regressions can be inspected without API credentials.

use std::{convert::Infallible, sync::Arc, time::Instant};

use decode::{
    agent::phase::AgentPhase,
    config::UiLanguage,
    lsp::{LspConnectionState, LspServerSnapshot},
    mcp::{McpConnectionState, McpServerSnapshot},
    ui::{app::AppState, i18n, render},
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    style::{Color, Style},
    widgets::Block,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GalleryTheme {
    Dark,
    Light,
}

fn infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => match error {},
    }
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let width = arguments
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(120);
    let height = arguments
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(40);
    let screen = arguments.next().unwrap_or_else(|| "chat".to_owned());
    let language = arguments.next().unwrap_or_else(|| "en".to_owned());
    let theme = match arguments.next().as_deref() {
        Some("light") => GalleryTheme::Light,
        _ => GalleryTheme::Dark,
    };
    i18n::set_language(parse_language(&language));

    let mut state = gallery_state();
    match screen.as_str() {
        "mcp" => state.mcp_ui.open(state.mcp_servers.len()),
        "mcp-add" => {
            state.mcp_ui.open(state.mcp_servers.len());
            state.mcp_ui.open_editor();
        }
        "lsp" => state
            .lsp_ui
            .open(state.lsp_servers.len(), state.lsp_diagnostics.len()),
        "lsp-add" => {
            state
                .lsp_ui
                .open(state.lsp_servers.len(), state.lsp_diagnostics.len());
            state.lsp_ui.open_editor();
        }
        _ => {}
    }

    let mut terminal = infallible(Terminal::new(TestBackend::new(width, height)));
    infallible(terminal.draw(|frame| {
        let base = match theme {
            GalleryTheme::Dark => Style::default().bg(Color::Black).fg(Color::White),
            GalleryTheme::Light => Style::default().bg(Color::White).fg(Color::Black),
        };
        frame.render_widget(Block::default().style(base), frame.area());
        render::draw(frame, &mut state);
    }));
    let buffer = terminal.backend().buffer();
    println!("# gallery theme={theme:?} language={language} size={width}x{height}");
    for row in 0..height {
        let mut line = String::new();
        for column in 0..width {
            line.push_str(buffer[(column, row)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

fn parse_language(value: &str) -> UiLanguage {
    match value.trim().to_ascii_lowercase().as_str() {
        "ru" | "russian" => UiLanguage::Russian,
        "uk" | "ua" | "ukrainian" => UiLanguage::Ukrainian,
        "es" | "spanish" => UiLanguage::Spanish,
        "de" | "german" => UiLanguage::German,
        "fr" | "french" => UiLanguage::French,
        "pl" | "polish" => UiLanguage::Polish,
        "pt" | "portuguese" => UiLanguage::Portuguese,
        "zh" | "chinese" => UiLanguage::Chinese,
        "ja" | "japanese" => UiLanguage::Japanese,
        "ko" | "korean" => UiLanguage::Korean,
        "tr" | "turkish" => UiLanguage::Turkish,
        _ => UiLanguage::English,
    }
}

fn gallery_state() -> AppState {
    let mut state = AppState::new();
    state.phase = AgentPhase::Streaming;
    state.deployment = "gpt-5.6-sol-primary".to_owned();
    state.connection_status = "streaming · bounded reconnect".to_owned();
    state.connected = true;
    state.context_mode = "stateless";
    state.context_budget = 200_000;
    state.max_context_budget = 2_000_000;
    state.live_thinking = "Inspecting the repository and validating invariants…".to_owned();
    state.mcp_servers = Arc::from([
        McpServerSnapshot {
            name: "documentation".to_owned(),
            transport: "HTTP",
            runtime_available: true,
            enabled: true,
            required: false,
            oauth: true,
            state: McpConnectionState::Connected,
            tool_count: 7,
            notice: decode::notice::UiNotice::McpToolsReady { count: 7 },
        },
        McpServerSnapshot {
            name: "local-database".to_owned(),
            transport: "STDIO",
            runtime_available: true,
            enabled: false,
            required: false,
            oauth: false,
            state: McpConnectionState::Disabled,
            tool_count: 0,
            notice: decode::notice::UiNotice::None,
        },
    ]);
    state.lsp_servers = Arc::from([LspServerSnapshot {
        name: "rust-analyzer".to_owned(),
        language_id: "rust".to_owned(),
        runtime_available: true,
        enabled: true,
        required: false,
        auto_start: true,
        detected: true,
        state: LspConnectionState::Connected,
        diagnostic_count: 2,
        notice: decode::notice::UiNotice::LspReady,
    }]);
    state.status_message =
        Some("Offline gallery · try: cargo run --example ui_gallery -- 120 40 mcp-add".to_owned());
    state.phase_started = Instant::now();
    state
}
