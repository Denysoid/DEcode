pub mod events;
pub mod live_preview;
pub mod tag_scanner;
pub mod tool_action;

pub use events::ParserEvent;
pub use live_preview::LivePreview;
pub use tag_scanner::{BlockTag, RawToolBlock, ScanError, ScanItem, TagScanner};
pub use tool_action::{ParseError, ToolAction, ToolOutcome, parse_tool_block};

/// Авторитетно разбирает полностью полученный ход модели.
///
/// Эту функцию разрешено вызывать только после подтверждённого
/// `response.completed`. Парсер намеренно не принимает поток дельт.
#[must_use]
pub fn parse_turn(source: &str) -> Vec<ParserEvent> {
    let mut events = Vec::new();
    let mut had_tool_calls = false;
    let mut tool_block_index = 0usize;

    for item in TagScanner::new(source) {
        match item {
            ScanItem::UnexpectedText { text, span } => {
                if !text.trim().is_empty() {
                    tracing::warn!(
                        span_start = span.start,
                        span_end = span.end,
                        byte_len = text.len(),
                        "Unexpected text outside known parser blocks"
                    );
                }
            }

            ScanItem::Block(block) => {
                if !block.tag.is_tool() {
                    continue;
                }

                had_tool_calls = true;
                tool_block_index = tool_block_index.saturating_add(1);

                let tag = block.tag;
                let raw_tag = block.raw;

                match parse_tool_block(block) {
                    Ok(action) => {
                        events.push(ParserEvent::ToolCallParsed(action));
                    }
                    Err(error) => {
                        events.push(ParserEvent::ToolCallParseError {
                            raw_tag: raw_tag.to_owned(),
                            reason: format!(
                                "failed to parse `{}` tool block #{}: {}",
                                tag.name(),
                                tool_block_index,
                                error
                            ),
                        });
                    }
                }
            }

            ScanItem::Error(error) => {
                let span_start = error.span().start;
                let span_end = error.span().end;

                match error.tag() {
                    Some(tag) if tag.is_tool() => {
                        had_tool_calls = true;
                        tool_block_index = tool_block_index.saturating_add(1);

                        events.push(ParserEvent::ToolCallParseError {
                            raw_tag: error.raw_tag().to_owned(),
                            reason: format!(
                                "failed to scan `{}` tool block #{}: {}",
                                tag.name(),
                                tool_block_index,
                                error
                            ),
                        });
                    }
                    tag => {
                        tracing::warn!(
                            tag = tag.map(BlockTag::name).unwrap_or("unknown"),
                            span_start,
                            span_end,
                            error = %error,
                            "Malformed non-tool parser block"
                        );
                    }
                }
            }
        }
    }

    events.push(ParserEvent::TurnComplete { had_tool_calls });
    events
}

/// Returns only user-facing prose, excluding thinking and tool protocol blocks.
#[must_use]
pub fn visible_assistant_text(source: &str) -> String {
    let mut visible = String::new();
    for item in TagScanner::new(source) {
        match item {
            ScanItem::UnexpectedText { text, .. } => visible.push_str(text),
            ScanItem::Block(_) => {}
            ScanItem::Error(error) => {
                // A completed but malformed tag is useful as visible diagnostics and
                // remains non-executable because authoritative parsing reports it.
                visible.push_str(error.raw_tag());
            }
        }
    }
    visible.trim().to_owned()
}
