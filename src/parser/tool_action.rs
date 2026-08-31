use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::tag_scanner::{BlockTag, RawToolBlock};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToolAction {
    ReadFile {
        path: String,
    },
    ListDirectory {
        path: String,
    },
    SearchCode {
        pattern: String,
        path: Option<String>,
    },
    ApplyPatch {
        path: String,
        search: String,
        replace: String,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ExecuteCommand {
        command: String,
        requires_confirmation: bool,
    },
}

impl ToolAction {
    #[must_use]
    pub const fn tool_name(&self) -> &'static str {
        match self {
            Self::ReadFile { .. } => "read_file",
            Self::ListDirectory { .. } => "list_directory",
            Self::SearchCode { .. } => "search_code",
            Self::ApplyPatch { .. } => "apply_patch",
            Self::WriteFile { .. } => "write_file",
            Self::ExecuteCommand { .. } => "execute_command",
        }
    }

    #[must_use]
    pub const fn requires_user_confirmation(&self) -> bool {
        match self {
            Self::ExecuteCommand {
                requires_confirmation,
                ..
            } => *requires_confirmation,
            _ => false,
        }
    }

    #[must_use]
    pub const fn is_mutating(&self) -> bool {
        matches!(
            self,
            Self::ApplyPatch { .. } | Self::WriteFile { .. } | Self::ExecuteCommand { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ToolOutcome {
    Success(String),
    Failure { message: String },
    Declined { action: ToolAction },
}

impl ToolOutcome {
    #[must_use]
    pub fn success(output: impl Into<String>) -> Self {
        Self::Success(output.into())
    }

    #[must_use]
    pub fn failure(message: impl Into<String>) -> Self {
        Self::Failure {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn declined(action: ToolAction) -> Self {
        Self::Declined { action }
    }

    #[must_use]
    pub const fn is_success(&self) -> bool {
        matches!(self, Self::Success(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("<thinking> is not a tool call")]
    ThinkingIsNotTool,

    #[error("missing required field <{field}> in <{tag}>")]
    MissingField { tag: BlockTag, field: &'static str },

    #[error("field <{field}> in <{tag}> has no closing tag")]
    UnclosedField { tag: BlockTag, field: &'static str },

    #[error("field <{field}> occurs more than once in <{tag}>")]
    DuplicateField { tag: BlockTag, field: &'static str },

    #[error("field <{field}> in <{tag}> must not be blank")]
    EmptyField { tag: BlockTag, field: &'static str },

    #[error("field <{field}> in <{tag}> contains nested field <{nested}>")]
    NestedField {
        tag: BlockTag,
        field: &'static str,
        nested: &'static str,
    },

    #[error("fields <{first}> and <{second}> overlap inside <{tag}>")]
    OverlappingFields {
        tag: BlockTag,
        first: &'static str,
        second: &'static str,
    },

    #[error("unexpected content at byte offset {offset} inside <{tag}>")]
    UnexpectedContent { tag: BlockTag, offset: usize },

    #[error("invalid UTF-8 string boundary while parsing <{tag}>")]
    InvalidBoundary { tag: BlockTag },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldSpec {
    name: &'static str,
    opening: &'static str,
    closing: &'static str,
}

const PATH: FieldSpec = FieldSpec {
    name: "path",
    opening: "<path>",
    closing: "</path>",
};

const PATTERN: FieldSpec = FieldSpec {
    name: "pattern",
    opening: "<pattern>",
    closing: "</pattern>",
};

const SEARCH: FieldSpec = FieldSpec {
    name: "search",
    opening: "<search>",
    closing: "</search>",
};

const REPLACE: FieldSpec = FieldSpec {
    name: "replace",
    opening: "<replace>",
    closing: "</replace>",
};

const CONTENT: FieldSpec = FieldSpec {
    name: "content",
    opening: "<content>",
    closing: "</content>",
};

const COMMAND: FieldSpec = FieldSpec {
    name: "command",
    opening: "<command>",
    closing: "</command>",
};

const REQUIRES_CONFIRMATION: FieldSpec = FieldSpec {
    name: "requires_confirmation",
    opening: "<requires_confirmation>",
    closing: "</requires_confirmation>",
};

const ALL_FIELDS: [FieldSpec; 7] = [
    PATH,
    PATTERN,
    SEARCH,
    REPLACE,
    CONTENT,
    COMMAND,
    REQUIRES_CONFIRMATION,
];

const CONFIRMATION_OPEN_PREFIX: &str = "<requires_confirmation";
const CONFIRMATION_CLOSE_PREFIX: &str = "</requires_confirmation";

#[derive(Debug, Clone, Copy)]
struct FieldMatch<'a> {
    spec: FieldSpec,
    value: &'a str,
    full_start: usize,
    full_end: usize,
}

fn locate_field<'a>(
    inner: &'a str,
    tag: BlockTag,
    spec: FieldSpec,
) -> Result<Option<FieldMatch<'a>>, ParseError> {
    let Some(full_start) = inner.find(spec.opening) else {
        return Ok(None);
    };

    let value_start = full_start
        .checked_add(spec.opening.len())
        .ok_or(ParseError::InvalidBoundary { tag })?;

    let after_opening = inner
        .get(value_start..)
        .ok_or(ParseError::InvalidBoundary { tag })?;

    let relative_value_end = after_opening
        .find(spec.closing)
        .ok_or(ParseError::UnclosedField {
            tag,
            field: spec.name,
        })?;

    let value_end = value_start
        .checked_add(relative_value_end)
        .ok_or(ParseError::InvalidBoundary { tag })?;

    let full_end = value_end
        .checked_add(spec.closing.len())
        .ok_or(ParseError::InvalidBoundary { tag })?;

    let value = inner
        .get(value_start..value_end)
        .ok_or(ParseError::InvalidBoundary { tag })?;

    Ok(Some(FieldMatch {
        spec,
        value,
        full_start,
        full_end,
    }))
}

fn required_unique_field<'a>(
    inner: &'a str,
    tag: BlockTag,
    spec: FieldSpec,
) -> Result<FieldMatch<'a>, ParseError> {
    let field = locate_field(inner, tag, spec)?.ok_or(ParseError::MissingField {
        tag,
        field: spec.name,
    })?;

    ensure_unique_field(inner, tag, field)?;
    Ok(field)
}

fn optional_unique_field<'a>(
    inner: &'a str,
    tag: BlockTag,
    spec: FieldSpec,
) -> Result<Option<FieldMatch<'a>>, ParseError> {
    let Some(field) = locate_field(inner, tag, spec)? else {
        return Ok(None);
    };

    ensure_unique_field(inner, tag, field)?;
    Ok(Some(field))
}

fn ensure_unique_field(
    inner: &str,
    tag: BlockTag,
    field: FieldMatch<'_>,
) -> Result<(), ParseError> {
    let trailing = inner
        .get(field.full_end..)
        .ok_or(ParseError::InvalidBoundary { tag })?;

    if trailing.contains(field.spec.opening) {
        return Err(ParseError::DuplicateField {
            tag,
            field: field.spec.name,
        });
    }

    Ok(())
}

fn validate_no_nested_fields(
    tag: BlockTag,
    field: FieldMatch<'_>,
    ignored: Option<FieldSpec>,
) -> Result<(), ParseError> {
    for candidate in ALL_FIELDS {
        if ignored == Some(candidate) {
            continue;
        }

        if field.value.contains(candidate.opening) {
            return Err(ParseError::NestedField {
                tag,
                field: field.spec.name,
                nested: candidate.name,
            });
        }
    }

    Ok(())
}

fn validate_layout<const N: usize>(
    inner: &str,
    tag: BlockTag,
    fields: &mut [FieldMatch<'_>; N],
) -> Result<(), ParseError> {
    for field in fields.iter().copied() {
        validate_no_nested_fields(tag, field, None)?;
    }

    fields.sort_unstable_by_key(|field| field.full_start);

    let mut cursor = 0usize;
    let mut previous: Option<FieldSpec> = None;

    for field in fields.iter().copied() {
        if field.full_start < cursor {
            let first = match previous {
                Some(spec) => spec.name,
                None => field.spec.name,
            };

            return Err(ParseError::OverlappingFields {
                tag,
                first,
                second: field.spec.name,
            });
        }

        let gap = inner
            .get(cursor..field.full_start)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        if !gap.trim().is_empty() {
            return Err(ParseError::UnexpectedContent {
                tag,
                offset: cursor,
            });
        }

        cursor = field.full_end;
        previous = Some(field.spec);
    }

    let trailing = inner
        .get(cursor..)
        .ok_or(ParseError::InvalidBoundary { tag })?;

    if !trailing.trim().is_empty() {
        return Err(ParseError::UnexpectedContent {
            tag,
            offset: cursor,
        });
    }

    Ok(())
}

fn trimmed_non_empty<'a>(tag: BlockTag, field: FieldMatch<'a>) -> Result<&'a str, ParseError> {
    let value = field.value.trim();

    if value.is_empty() {
        return Err(ParseError::EmptyField {
            tag,
            field: field.spec.name,
        });
    }

    Ok(value)
}

fn preserved_non_blank<'a>(tag: BlockTag, field: FieldMatch<'a>) -> Result<&'a str, ParseError> {
    if field.value.trim().is_empty() {
        return Err(ParseError::EmptyField {
            tag,
            field: field.spec.name,
        });
    }

    Ok(field.value)
}

#[derive(Debug, Default)]
struct ConfirmationSummary {
    occurrences: usize,
    malformed: bool,
    single_value: Option<bool>,
}

impl ConfirmationSummary {
    fn record(&mut self, value: Option<bool>) {
        if self.occurrences == 0 {
            self.single_value = value;
        } else {
            self.single_value = None;
            self.malformed = true;
        }

        self.occurrences = self.occurrences.saturating_add(1);

        if value.is_none() {
            self.malformed = true;
        }
    }

    const fn requires_confirmation(&self) -> bool {
        !matches!(
            (self.occurrences, self.malformed, self.single_value),
            (1, false, Some(false))
        )
    }
}

fn checked_offset(tag: BlockTag, base: usize, local: usize) -> Result<usize, ParseError> {
    base.checked_add(local)
        .ok_or(ParseError::InvalidBoundary { tag })
}

fn validate_malformed_confirmation_fragment(
    fragment: &str,
    tag: BlockTag,
    base_offset: usize,
) -> Result<(), ParseError> {
    for field in ALL_FIELDS {
        if field != REQUIRES_CONFIRMATION && fragment.contains(field.opening) {
            return Err(ParseError::NestedField {
                tag,
                field: REQUIRES_CONFIRMATION.name,
                nested: field.name,
            });
        }
    }

    let mut cursor = 0usize;

    loop {
        let remaining = fragment
            .get(cursor..)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        let Some(relative_start) = remaining.find('<') else {
            break;
        };

        let marker_start = cursor
            .checked_add(relative_start)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        let marker_tail = fragment
            .get(marker_start..)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        if !marker_tail.starts_with(CONFIRMATION_OPEN_PREFIX)
            && !marker_tail.starts_with(CONFIRMATION_CLOSE_PREFIX)
        {
            return Err(ParseError::UnexpectedContent {
                tag,
                offset: checked_offset(tag, base_offset, marker_start)?,
            });
        }

        cursor = marker_start
            .checked_add(1)
            .ok_or(ParseError::InvalidBoundary { tag })?;
    }

    Ok(())
}

fn parse_confirmation_area(
    area: &str,
    tag: BlockTag,
    base_offset: usize,
    summary: &mut ConfirmationSummary,
) -> Result<(), ParseError> {
    let mut cursor = 0usize;

    loop {
        let remaining = area
            .get(cursor..)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        let trimmed = remaining.trim_start();
        let skipped = remaining.len().saturating_sub(trimmed.len());

        cursor = cursor
            .checked_add(skipped)
            .ok_or(ParseError::InvalidBoundary { tag })?;

        if trimmed.is_empty() {
            return Ok(());
        }

        if trimmed.starts_with(REQUIRES_CONFIRMATION.opening) {
            let value_start = cursor
                .checked_add(REQUIRES_CONFIRMATION.opening.len())
                .ok_or(ParseError::InvalidBoundary { tag })?;

            let after_opening = area
                .get(value_start..)
                .ok_or(ParseError::InvalidBoundary { tag })?;

            let Some(relative_value_end) = after_opening.find(REQUIRES_CONFIRMATION.closing) else {
                let malformed = area
                    .get(cursor..)
                    .ok_or(ParseError::InvalidBoundary { tag })?;

                validate_malformed_confirmation_fragment(
                    malformed,
                    tag,
                    checked_offset(tag, base_offset, cursor)?,
                )?;

                summary.record(None);
                return Ok(());
            };

            let value_end = value_start
                .checked_add(relative_value_end)
                .ok_or(ParseError::InvalidBoundary { tag })?;

            let full_end = value_end
                .checked_add(REQUIRES_CONFIRMATION.closing.len())
                .ok_or(ParseError::InvalidBoundary { tag })?;

            let value = area
                .get(value_start..value_end)
                .ok_or(ParseError::InvalidBoundary { tag })?;

            let confirmation_field = FieldMatch {
                spec: REQUIRES_CONFIRMATION,
                value,
                full_start: cursor,
                full_end,
            };

            validate_no_nested_fields(tag, confirmation_field, Some(REQUIRES_CONFIRMATION))?;

            let parsed = match value.trim() {
                "true" => Some(true),
                "false" => Some(false),
                _ => None,
            };

            summary.record(parsed);
            cursor = full_end;
            continue;
        }

        if trimmed.starts_with(CONFIRMATION_OPEN_PREFIX)
            || trimmed.starts_with(CONFIRMATION_CLOSE_PREFIX)
        {
            validate_malformed_confirmation_fragment(
                trimmed,
                tag,
                checked_offset(tag, base_offset, cursor)?,
            )?;

            summary.record(None);
            return Ok(());
        }

        return Err(ParseError::UnexpectedContent {
            tag,
            offset: checked_offset(tag, base_offset, cursor)?,
        });
    }
}

fn parse_read_file(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let path = required_unique_field(block.inner, block.tag, PATH)?;
    let mut fields = [path];

    validate_layout(block.inner, block.tag, &mut fields)?;

    Ok(ToolAction::ReadFile {
        path: trimmed_non_empty(block.tag, path)?.to_owned(),
    })
}

fn parse_list_directory(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let path = required_unique_field(block.inner, block.tag, PATH)?;
    let mut fields = [path];

    validate_layout(block.inner, block.tag, &mut fields)?;

    Ok(ToolAction::ListDirectory {
        path: trimmed_non_empty(block.tag, path)?.to_owned(),
    })
}

fn parse_search_code(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let pattern = required_unique_field(block.inner, block.tag, PATTERN)?;
    let path = optional_unique_field(block.inner, block.tag, PATH)?;

    match path {
        Some(path_field) => {
            let mut fields = [pattern, path_field];
            validate_layout(block.inner, block.tag, &mut fields)?;

            Ok(ToolAction::SearchCode {
                pattern: preserved_non_blank(block.tag, pattern)?.to_owned(),
                path: Some(trimmed_non_empty(block.tag, path_field)?.to_owned()),
            })
        }
        None => {
            let mut fields = [pattern];
            validate_layout(block.inner, block.tag, &mut fields)?;

            Ok(ToolAction::SearchCode {
                pattern: preserved_non_blank(block.tag, pattern)?.to_owned(),
                path: None,
            })
        }
    }
}

fn parse_apply_patch(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let path = required_unique_field(block.inner, block.tag, PATH)?;
    let search = required_unique_field(block.inner, block.tag, SEARCH)?;
    let replace = required_unique_field(block.inner, block.tag, REPLACE)?;

    let mut fields = [path, search, replace];
    validate_layout(block.inner, block.tag, &mut fields)?;

    Ok(ToolAction::ApplyPatch {
        path: trimmed_non_empty(block.tag, path)?.to_owned(),
        search: preserved_non_blank(block.tag, search)?.to_owned(),
        replace: replace.value.to_owned(),
    })
}

fn parse_write_file(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let path = required_unique_field(block.inner, block.tag, PATH)?;
    let content = required_unique_field(block.inner, block.tag, CONTENT)?;

    let mut fields = [path, content];
    validate_layout(block.inner, block.tag, &mut fields)?;

    Ok(ToolAction::WriteFile {
        path: trimmed_non_empty(block.tag, path)?.to_owned(),
        content: content.value.to_owned(),
    })
}

fn parse_execute_command(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    let command = required_unique_field(block.inner, block.tag, COMMAND)?;

    validate_no_nested_fields(block.tag, command, None)?;

    let command_value = preserved_non_blank(block.tag, command)?;

    let prefix = block
        .inner
        .get(..command.full_start)
        .ok_or(ParseError::InvalidBoundary { tag: block.tag })?;

    let suffix = block
        .inner
        .get(command.full_end..)
        .ok_or(ParseError::InvalidBoundary { tag: block.tag })?;

    let mut confirmation = ConfirmationSummary::default();

    parse_confirmation_area(prefix, block.tag, 0, &mut confirmation)?;

    parse_confirmation_area(suffix, block.tag, command.full_end, &mut confirmation)?;

    Ok(ToolAction::ExecuteCommand {
        command: decode_standard_entities(command_value),
        requires_confirmation: confirmation.requires_confirmation(),
    })
}

fn decode_standard_entities(value: &str) -> String {
    if !value.contains('&') {
        return value.to_owned();
    }
    value
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

pub fn parse_tool_block(block: RawToolBlock<'_>) -> Result<ToolAction, ParseError> {
    match block.tag {
        BlockTag::Thinking => Err(ParseError::ThinkingIsNotTool),
        BlockTag::ReadFile => parse_read_file(block),
        BlockTag::ListDirectory => parse_list_directory(block),
        BlockTag::SearchCode => parse_search_code(block),
        BlockTag::ApplyPatch => parse_apply_patch(block),
        BlockTag::WriteFile => parse_write_file(block),
        BlockTag::ExecuteCommand => parse_execute_command(block),
    }
}
