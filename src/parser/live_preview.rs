use super::events::ParserEvent;

const THINKING_OPEN: &str = "<thinking>";
const THINKING_CLOSE: &str = "</thinking>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewState {
    OutsideThinking,
    InsideThinking,
}

/// Косметический инкрементальный scanner только для `<thinking>`.
///
/// Он никогда не создаёт `ToolAction` и не участвует в авторитетном
/// разборе инструментов.
#[derive(Debug, Clone)]
pub struct LivePreview {
    state: PreviewState,
    pending: String,
}

impl LivePreview {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: PreviewState::OutsideThinking,
            pending: String::new(),
        }
    }

    #[must_use]
    pub const fn is_inside_thinking(&self) -> bool {
        matches!(self.state, PreviewState::InsideThinking)
    }

    pub fn feed(&mut self, delta: &str) -> Vec<ParserEvent> {
        self.pending.push_str(delta);

        let mut events = Vec::new();
        let mut cursor = 0usize;

        let drain_until = loop {
            let Some(remaining) = self.pending.get(cursor..) else {
                self.reset();
                return events;
            };

            match self.state {
                PreviewState::OutsideThinking => {
                    let Some(relative_opening) = remaining.find(THINKING_OPEN) else {
                        let suffix_length = longest_marker_prefix_suffix(remaining, THINKING_OPEN);

                        let Some(position) = self.pending.len().checked_sub(suffix_length) else {
                            self.reset();
                            return events;
                        };

                        break position;
                    };

                    let Some(opening_start) = cursor.checked_add(relative_opening) else {
                        self.reset();
                        return events;
                    };

                    let Some(after_opening) = opening_start.checked_add(THINKING_OPEN.len()) else {
                        self.reset();
                        return events;
                    };

                    cursor = after_opening;
                    self.state = PreviewState::InsideThinking;
                }

                PreviewState::InsideThinking => {
                    if let Some(relative_closing) = remaining.find(THINKING_CLOSE) {
                        let Some(closing_start) = cursor.checked_add(relative_closing) else {
                            self.reset();
                            return events;
                        };

                        let Some(thinking_text) = self.pending.get(cursor..closing_start) else {
                            self.reset();
                            return events;
                        };

                        if !thinking_text.is_empty() {
                            events.push(ParserEvent::ThinkingDelta(thinking_text.to_owned()));
                        }

                        let Some(after_closing) = closing_start.checked_add(THINKING_CLOSE.len())
                        else {
                            self.reset();
                            return events;
                        };

                        cursor = after_closing;
                        self.state = PreviewState::OutsideThinking;
                        events.push(ParserEvent::ThinkingEnd);
                    } else {
                        let suffix_length = longest_marker_prefix_suffix(remaining, THINKING_CLOSE);

                        let Some(emit_end) = self.pending.len().checked_sub(suffix_length) else {
                            self.reset();
                            return events;
                        };

                        let Some(thinking_text) = self.pending.get(cursor..emit_end) else {
                            self.reset();
                            return events;
                        };

                        if !thinking_text.is_empty() {
                            events.push(ParserEvent::ThinkingDelta(thinking_text.to_owned()));
                        }

                        break emit_end;
                    }
                }
            }
        };

        if self.pending.get(..drain_until).is_none() {
            self.reset();
            return events;
        }

        self.pending.replace_range(..drain_until, "");
        events
    }

    /// Завершает косметический preview.
    ///
    /// Незакрытый `<thinking>` не получает искусственный `ThinkingEnd`.
    /// Сохранённый префикс закрывающего тега возвращается как обычный
    /// thinking-текст, после чего внутреннее состояние сбрасывается.
    pub fn finish(&mut self) -> Vec<ParserEvent> {
        let mut events = Vec::new();

        if self.is_inside_thinking() && !self.pending.is_empty() {
            events.push(ParserEvent::ThinkingDelta(std::mem::take(
                &mut self.pending,
            )));
        } else {
            self.pending.clear();
        }

        self.state = PreviewState::OutsideThinking;
        events
    }

    pub fn reset(&mut self) {
        self.state = PreviewState::OutsideThinking;
        self.pending.clear();
    }
}

impl Default for LivePreview {
    fn default() -> Self {
        Self::new()
    }
}

fn longest_marker_prefix_suffix(text: &str, marker: &str) -> usize {
    let mut length = marker.len().saturating_sub(1).min(text.len());

    while length > 0 {
        let prefix = match marker.get(..length) {
            Some(value) => value,
            None => {
                length = length.saturating_sub(1);
                continue;
            }
        };

        if text.ends_with(prefix) {
            return length;
        }

        length = length.saturating_sub(1);
    }

    0
}
