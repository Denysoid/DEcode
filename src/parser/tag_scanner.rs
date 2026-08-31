use std::{fmt, iter::FusedIterator, ops::Range};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTag {
    Thinking,
    ReadFile,
    ListDirectory,
    SearchCode,
    ApplyPatch,
    WriteFile,
    ExecuteCommand,
}

impl BlockTag {
    const ALL: [Self; 7] = [
        Self::Thinking,
        Self::ReadFile,
        Self::ListDirectory,
        Self::SearchCode,
        Self::ApplyPatch,
        Self::WriteFile,
        Self::ExecuteCommand,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Thinking => "thinking",
            Self::ReadFile => "read_file",
            Self::ListDirectory => "list_directory",
            Self::SearchCode => "search_code",
            Self::ApplyPatch => "apply_patch",
            Self::WriteFile => "write_file",
            Self::ExecuteCommand => "execute_command",
        }
    }

    #[must_use]
    pub const fn opening(self) -> &'static str {
        match self {
            Self::Thinking => "<thinking>",
            Self::ReadFile => "<read_file>",
            Self::ListDirectory => "<list_directory>",
            Self::SearchCode => "<search_code>",
            Self::ApplyPatch => "<apply_patch>",
            Self::WriteFile => "<write_file>",
            Self::ExecuteCommand => "<execute_command>",
        }
    }

    #[must_use]
    pub const fn closing(self) -> &'static str {
        match self {
            Self::Thinking => "</thinking>",
            Self::ReadFile => "</read_file>",
            Self::ListDirectory => "</list_directory>",
            Self::SearchCode => "</search_code>",
            Self::ApplyPatch => "</apply_patch>",
            Self::WriteFile => "</write_file>",
            Self::ExecuteCommand => "</execute_command>",
        }
    }

    #[must_use]
    pub const fn is_tool(self) -> bool {
        !matches!(self, Self::Thinking)
    }
}

impl fmt::Display for BlockTag {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Заимствованный блок исходного ответа.
///
/// Сканер не копирует полный блок и не копирует `inner`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawToolBlock<'a> {
    pub tag: BlockTag,
    pub inner: &'a str,
    pub raw: &'a str,
    pub span: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScanError<'a> {
    #[error("unclosed <{tag}> block: expected literal closing tag {closing}")]
    MissingClosingTag {
        tag: BlockTag,
        closing: &'static str,
        raw_tag: &'a str,
        span: Range<usize>,
    },

    #[error("invalid UTF-8 string boundary while scanning parser input")]
    InvalidBoundary {
        tag: Option<BlockTag>,
        raw_tag: &'a str,
        span: Range<usize>,
    },
}

impl<'a> ScanError<'a> {
    #[must_use]
    pub const fn tag(&self) -> Option<BlockTag> {
        match self {
            Self::MissingClosingTag { tag, .. } => Some(*tag),
            Self::InvalidBoundary { tag, .. } => *tag,
        }
    }

    #[must_use]
    pub const fn raw_tag(&self) -> &'a str {
        match self {
            Self::MissingClosingTag { raw_tag, .. } | Self::InvalidBoundary { raw_tag, .. } => {
                raw_tag
            }
        }
    }

    #[must_use]
    pub const fn span(&self) -> &Range<usize> {
        match self {
            Self::MissingClosingTag { span, .. } | Self::InvalidBoundary { span, .. } => span,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanItem<'a> {
    UnexpectedText { text: &'a str, span: Range<usize> },
    Block(RawToolBlock<'a>),
    Error(ScanError<'a>),
}

/// Безаллокирующий авторитетный сканер полного хода.
///
/// Сканер ищет только литеральные строки известных внешних тегов.
/// Неизвестные теги не интерпретируются и входят в `UnexpectedText`.
#[derive(Debug, Clone)]
pub struct TagScanner<'a> {
    source: &'a str,
    cursor: usize,
    finished: bool,
}

impl<'a> TagScanner<'a> {
    #[must_use]
    pub const fn new(source: &'a str) -> Self {
        Self {
            source,
            cursor: 0,
            finished: false,
        }
    }

    fn find_next_opening(&self) -> Option<(usize, BlockTag)> {
        let mut search_from = self.cursor;

        loop {
            let remaining = self.source.get(search_from..)?;
            let relative_start = remaining.find('<')?;
            let opening_start = search_from.checked_add(relative_start)?;
            let tail = self.source.get(opening_start..)?;

            for tag in BlockTag::ALL {
                if tail.starts_with(tag.opening()) {
                    return Some((opening_start, tag));
                }
            }

            search_from = opening_start.checked_add(1)?;
        }
    }

    fn invalid_boundary(&mut self, tag: Option<BlockTag>, requested_start: usize) -> ScanItem<'a> {
        self.finished = true;
        self.cursor = self.source.len();

        let (raw_tag, span_start) = match self.source.get(requested_start..) {
            Some(raw) => (raw, requested_start),
            None => ("", self.source.len()),
        };

        ScanItem::Error(ScanError::InvalidBoundary {
            tag,
            raw_tag,
            span: span_start..self.source.len(),
        })
    }

    fn missing_closing(&mut self, tag: BlockTag, opening_start: usize) -> ScanItem<'a> {
        self.finished = true;
        self.cursor = self.source.len();

        let raw_tag = self.source.get(opening_start..).unwrap_or_default();

        ScanItem::Error(ScanError::MissingClosingTag {
            tag,
            closing: tag.closing(),
            raw_tag,
            span: opening_start..self.source.len(),
        })
    }
}

impl<'a> Iterator for TagScanner<'a> {
    type Item = ScanItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }

        if self.cursor >= self.source.len() {
            self.finished = true;
            return None;
        }

        let Some((opening_start, tag)) = self.find_next_opening() else {
            let text = match self.source.get(self.cursor..) {
                Some(value) => value,
                None => {
                    return Some(self.invalid_boundary(None, self.cursor));
                }
            };

            let span = self.cursor..self.source.len();
            self.cursor = self.source.len();
            self.finished = true;

            return Some(ScanItem::UnexpectedText { text, span });
        };

        if opening_start > self.cursor {
            let text = match self.source.get(self.cursor..opening_start) {
                Some(value) => value,
                None => {
                    return Some(self.invalid_boundary(None, self.cursor));
                }
            };

            let span = self.cursor..opening_start;
            self.cursor = opening_start;

            return Some(ScanItem::UnexpectedText { text, span });
        }

        let opening_end = match opening_start.checked_add(tag.opening().len()) {
            Some(position) => position,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        let after_opening = match self.source.get(opening_end..) {
            Some(value) => value,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        let Some(relative_closing_start) = after_opening.find(tag.closing()) else {
            return Some(self.missing_closing(tag, opening_start));
        };

        let closing_start = match opening_end.checked_add(relative_closing_start) {
            Some(position) => position,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        let block_end = match closing_start.checked_add(tag.closing().len()) {
            Some(position) => position,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        let inner = match self.source.get(opening_end..closing_start) {
            Some(value) => value,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        let raw = match self.source.get(opening_start..block_end) {
            Some(value) => value,
            None => {
                return Some(self.invalid_boundary(Some(tag), opening_start));
            }
        };

        self.cursor = block_end;

        Some(ScanItem::Block(RawToolBlock {
            tag,
            inner,
            raw,
            span: opening_start..block_end,
        }))
    }
}

impl FusedIterator for TagScanner<'_> {}
