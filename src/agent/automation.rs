use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::tools::{SandboxRoot, exec};

const FORMAT_VERSION: u32 = 1;
const MAX_FILES_PER_KIND: usize = 64;
const MAX_DEFINITION_BYTES: u64 = 128 * 1024;
const MAX_TEMPLATE_BYTES: usize = 64 * 1024;
const MAX_EXPANDED_BYTES: usize = MAX_TEMPLATE_BYTES * 2;
const MAX_ID_BYTES: usize = 64;
const MAX_NAME_BYTES: usize = 96;
const MAX_DESCRIPTION_BYTES: usize = 1_024;
const MAX_ARGUMENTS: usize = 32;
const MAX_ARGUMENT_BYTES: usize = 8 * 1024;
const MAX_HOOK_ARGS: usize = 32;
const MAX_HOOK_ARG_BYTES: usize = 4 * 1024;
const MAX_HOOK_TIMEOUT_MS: u64 = 30_000;
const MAX_HOOK_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutomationSource {
    User,
    Project,
}

impl std::fmt::Display for AutomationSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::User => "user",
            Self::Project => "project",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    TurnComplete,
}

impl std::fmt::Display for HookEvent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::SessionStart => "session start",
            Self::UserPromptSubmit => "user prompt",
            Self::PreToolUse => "before tool",
            Self::PostToolUse => "after tool",
            Self::TurnComplete => "turn complete",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: AutomationSource,
    pub source_path: PathBuf,
    pub argument_hint: String,
    pub requires_arguments: bool,
    template: String,
}

impl CustomCommand {
    #[must_use]
    pub fn summary(&self) -> CustomCommandSummary {
        CustomCommandSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            source: self.source,
            source_path: self.source_path.clone(),
            argument_hint: self.argument_hint.clone(),
            requires_arguments: self.requires_arguments,
        }
    }

    fn expand(&self, raw_arguments: &str) -> Result<String, AutomationError> {
        let raw_arguments = raw_arguments.trim();
        if self.requires_arguments && raw_arguments.is_empty() {
            return Err(AutomationError::ArgumentsRequired {
                command: self.id.clone(),
                hint: self.argument_hint.clone(),
            });
        }
        let positional = split_arguments(raw_arguments)?;
        let (mut expanded, has_any_placeholder) =
            expand_template(&self.template, raw_arguments, &positional, &self.id)?;
        if !has_any_placeholder && !raw_arguments.is_empty() {
            push_expanded(&mut expanded, "\n\nArguments: ", &self.id)?;
            push_expanded(&mut expanded, raw_arguments, &self.id)?;
        }
        Ok(expanded)
    }
}

fn expand_template(
    template: &str,
    raw_arguments: &str,
    positional: &[String],
    command: &str,
) -> Result<(String, bool), AutomationError> {
    let mut expanded = String::with_capacity(template.len().min(MAX_EXPANDED_BYTES));
    let mut cursor = 0_usize;
    let mut found_placeholder = false;
    while let Some(relative) = template[cursor..].find('$') {
        let marker_start = cursor.saturating_add(relative);
        push_expanded(&mut expanded, &template[cursor..marker_start], command)?;
        let tail = &template[marker_start..];
        let replacement = if tail.starts_with("$ARGUMENTS") {
            Some((raw_arguments, "$ARGUMENTS".len()))
        } else if tail.starts_with("${ARGS}") {
            Some((raw_arguments, "${ARGS}".len()))
        } else {
            positional_marker(tail, positional, command)?
        };
        if let Some((value, marker_bytes)) = replacement {
            found_placeholder = true;
            push_expanded(&mut expanded, value, command)?;
            cursor = marker_start.saturating_add(marker_bytes);
        } else {
            push_expanded(&mut expanded, "$", command)?;
            cursor = marker_start.saturating_add(1);
        }
    }
    push_expanded(&mut expanded, &template[cursor..], command)?;
    Ok((expanded, found_placeholder))
}

fn positional_marker<'a>(
    tail: &str,
    positional: &'a [String],
    command: &str,
) -> Result<Option<(&'a str, usize)>, AutomationError> {
    let bytes = tail.as_bytes();
    if bytes.len() < 4
        || bytes[0] != b'$'
        || bytes[1] != b'{'
        || !matches!(bytes[2], b'1'..=b'9')
        || bytes[3] != b'}'
    {
        return Ok(None);
    }
    let position = usize::from(bytes[2] - b'0');
    let value =
        positional
            .get(position - 1)
            .ok_or_else(|| AutomationError::MissingPositionalArgument {
                command: command.to_owned(),
                position,
            })?;
    Ok(Some((value, 4)))
}

fn push_expanded(target: &mut String, value: &str, command: &str) -> Result<(), AutomationError> {
    if target
        .len()
        .checked_add(value.len())
        .is_none_or(|length| length > MAX_EXPANDED_BYTES)
    {
        return Err(AutomationError::ExpandedCommandTooLarge {
            command: command.to_owned(),
        });
    }
    target.push_str(value);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommandSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: AutomationSource,
    pub source_path: PathBuf,
    pub argument_hint: String,
    pub requires_arguments: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDefinition {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
    pub event: HookEvent,
    pub program: PathBuf,
    pub args: Arc<[String]>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub blocking: bool,
    pub enabled: bool,
    pub tool_match: Arc<[String]>,
}

impl HookDefinition {
    #[must_use]
    pub fn summary(&self) -> HookSummary {
        HookSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            source_path: self.source_path.clone(),
            event: self.event,
            program: self.program.clone(),
            args: Arc::clone(&self.args),
            timeout: self.timeout,
            blocking: self.blocking,
            enabled: self.enabled,
            tool_match: Arc::clone(&self.tool_match),
        }
    }

    #[must_use]
    pub fn matches_tool(&self, tool_name: Option<&str>) -> bool {
        if !matches!(self.event, HookEvent::PreToolUse | HookEvent::PostToolUse) {
            return true;
        }
        let Some(tool_name) = tool_name else {
            return false;
        };
        self.tool_match
            .iter()
            .any(|matcher| matcher == "*" || matcher == tool_name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source_path: PathBuf,
    pub event: HookEvent,
    pub program: PathBuf,
    pub args: Arc<[String]>,
    pub timeout: Duration,
    pub blocking: bool,
    pub enabled: bool,
    pub tool_match: Arc<[String]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationSnapshot {
    pub revision: u64,
    pub user_commands_dir: Option<PathBuf>,
    pub project_commands_dir: PathBuf,
    pub user_hooks_dir: Option<PathBuf>,
    pub commands: Arc<[CustomCommandSummary]>,
    pub hooks: Arc<[HookSummary]>,
    pub diagnostics: Arc<[String]>,
}

impl Default for AutomationSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            user_commands_dir: None,
            project_commands_dir: PathBuf::new(),
            user_hooks_dir: None,
            commands: Arc::from([]),
            hooks: Arc::from([]),
            diagnostics: Arc::from([]),
        }
    }
}

#[derive(Debug, Error)]
pub enum AutomationError {
    #[error("unknown custom command /{0}")]
    UnknownCommand(String),
    #[error("custom command /{command} requires arguments ({hint})")]
    ArgumentsRequired { command: String, hint: String },
    #[error("custom command /{command} requires positional argument {position}")]
    MissingPositionalArgument { command: String, position: usize },
    #[error("custom command /{command} expands beyond the configured size limit")]
    ExpandedCommandTooLarge { command: String },
    #[error("custom command arguments contain an unclosed quote")]
    UnclosedQuote,
    #[error("custom command has more than {MAX_ARGUMENTS} arguments")]
    TooManyArguments,
    #[error("custom command argument exceeds {MAX_ARGUMENT_BYTES} bytes")]
    ArgumentTooLarge,
    #[error("unknown lifecycle hook {0:?}")]
    UnknownHook(String),
}

#[derive(Debug)]
pub struct AutomationCatalog {
    workspace_root: PathBuf,
    user_root: Option<PathBuf>,
    revision: u64,
    commands: BTreeMap<String, CustomCommand>,
    hooks: BTreeMap<String, HookDefinition>,
    hook_overrides: BTreeMap<String, bool>,
    diagnostics: Vec<String>,
}

impl AutomationCatalog {
    #[must_use]
    pub fn load(workspace_root: PathBuf) -> Self {
        let user_root = directories::ProjectDirs::from("dev", "denysoid", "decode")
            .map(|directories| directories.config_dir().to_path_buf());
        Self::load_from(workspace_root, user_root)
    }

    fn load_from(workspace_root: PathBuf, user_root: Option<PathBuf>) -> Self {
        let mut catalog = Self {
            workspace_root,
            user_root,
            revision: 0,
            commands: BTreeMap::new(),
            hooks: BTreeMap::new(),
            hook_overrides: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        catalog.reload();
        catalog
    }

    #[cfg(test)]
    pub(crate) fn load_from_for_test(workspace_root: PathBuf, user_root: Option<PathBuf>) -> Self {
        Self::load_from(workspace_root, user_root)
    }

    pub fn reload(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.commands.clear();
        self.hooks.clear();
        self.diagnostics.clear();

        let project_commands = self.workspace_root.join(".decode").join("commands");
        self.load_commands(&project_commands, AutomationSource::Project, false);
        if let Some(user_root) = self.user_root.clone() {
            self.load_commands(&user_root.join("commands"), AutomationSource::User, true);
            self.load_hooks(&user_root.join("hooks"));
        }
        self.report_ignored_project_hooks();
        self.hook_overrides
            .retain(|id, _| self.hooks.contains_key(id));
        for (id, enabled) in &self.hook_overrides {
            if let Some(hook) = self.hooks.get_mut(id) {
                hook.enabled = *enabled;
            }
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> AutomationSnapshot {
        AutomationSnapshot {
            revision: self.revision,
            user_commands_dir: self.user_root.as_ref().map(|root| root.join("commands")),
            project_commands_dir: self.workspace_root.join(".decode").join("commands"),
            user_hooks_dir: self.user_root.as_ref().map(|root| root.join("hooks")),
            commands: self
                .commands
                .values()
                .map(CustomCommand::summary)
                .collect::<Vec<_>>()
                .into(),
            hooks: self
                .hooks
                .values()
                .map(HookDefinition::summary)
                .collect::<Vec<_>>()
                .into(),
            diagnostics: self.diagnostics.clone().into(),
        }
    }

    pub fn set_hook_enabled(&mut self, id: &str, enabled: bool) -> Result<(), AutomationError> {
        let hook = self
            .hooks
            .get_mut(id)
            .ok_or_else(|| AutomationError::UnknownHook(id.to_owned()))?;
        hook.enabled = enabled;
        self.hook_overrides.insert(id.to_owned(), enabled);
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn expand_invocation(&self, raw: &str) -> Result<Option<String>, AutomationError> {
        let Some(invocation) = raw.strip_prefix('/') else {
            return Ok(None);
        };
        let id_end = invocation
            .find(char::is_whitespace)
            .unwrap_or(invocation.len());
        if id_end == 0 {
            return Ok(None);
        }
        let id = &invocation[..id_end];
        let Some(command) = self.commands.get(id) else {
            return Ok(None);
        };
        let arguments = invocation[id_end..].trim_start();
        command.expand(arguments).map(Some)
    }

    #[must_use]
    pub fn matching_hooks(&self, event: HookEvent, tool_name: Option<&str>) -> Vec<HookDefinition> {
        self.hooks
            .values()
            .filter(|hook| hook.enabled && hook.event == event && hook.matches_tool(tool_name))
            .cloned()
            .collect()
    }

    fn load_commands(&mut self, directory: &Path, source: AutomationSource, replace: bool) {
        for path in definition_paths(directory, "command", &mut self.diagnostics) {
            match load_command(&path, directory, source) {
                Ok(command) => {
                    if self.commands.contains_key(&command.id) && !replace {
                        self.diagnostics.push(format!(
                            "duplicate custom command /{} at {:?}; first definition remains active",
                            command.id, path
                        ));
                    } else {
                        if self.commands.contains_key(&command.id) {
                            self.diagnostics.push(format!(
                                "trusted user command /{} overrides the project command",
                                command.id
                            ));
                        }
                        self.commands.insert(command.id.clone(), command);
                    }
                }
                Err(error) => self
                    .diagnostics
                    .push(format!("custom command {:?} was skipped: {error}", path)),
            }
        }
    }

    fn load_hooks(&mut self, directory: &Path) {
        for path in definition_paths(directory, "hook", &mut self.diagnostics) {
            match load_hook(&path) {
                Ok(hook) => {
                    if self.hooks.contains_key(&hook.id) {
                        self.diagnostics.push(format!(
                            "duplicate lifecycle hook {:?} at {:?}; first definition remains active",
                            hook.id, path
                        ));
                    } else {
                        self.hooks.insert(hook.id.clone(), hook);
                    }
                }
                Err(error) => self
                    .diagnostics
                    .push(format!("lifecycle hook {:?} was skipped: {error}", path)),
            }
        }
    }

    fn report_ignored_project_hooks(&mut self) {
        let directory = self.workspace_root.join(".decode").join("hooks");
        let has_toml = fs::read_dir(&directory).ok().is_some_and(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "toml")
            })
        });
        if has_toml {
            self.diagnostics.push(format!(
                "project executable hooks in {:?} are ignored; copy reviewed hooks to the trusted user hooks directory",
                directory
            ));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDisposition {
    Continue,
    Deny { hook_id: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunReport {
    pub disposition: HookDisposition,
    pub notes: Arc<[String]>,
}

pub async fn run_hooks(
    hooks: Vec<HookDefinition>,
    event: HookEvent,
    payload: &Value,
    sandbox: &SandboxRoot,
    cancel: &CancellationToken,
) -> HookRunReport {
    let payload = match serde_json::to_vec(payload) {
        Ok(payload) => payload,
        Err(error) => {
            return HookRunReport {
                disposition: HookDisposition::Deny {
                    hook_id: "harness".to_owned(),
                    message: format!("hook payload could not be serialized: {error}"),
                },
                notes: Arc::from([]),
            };
        }
    };
    let mut notes = Vec::new();
    for hook in hooks {
        let result = exec::execute_trusted_direct(
            sandbox,
            &hook.program,
            hook.args.as_ref(),
            &payload,
            hook.timeout,
            hook.max_output_bytes,
            cancel.child_token(),
        )
        .await;
        match result {
            Ok(output) => {
                if !output.trim().is_empty() {
                    notes.push(format!(
                        "{}: {}",
                        hook.name,
                        bounded_note(output.trim(), 2_000)
                    ));
                }
            }
            Err(exec::ExecError::NonZeroExit {
                code: Some(2),
                output,
            }) if event_can_deny(event) => {
                let message = if output.trim().is_empty() {
                    format!("{} denied the tool action", hook.name)
                } else {
                    bounded_note(output.trim(), 2_000)
                };
                return HookRunReport {
                    disposition: HookDisposition::Deny {
                        hook_id: hook.id,
                        message,
                    },
                    notes: notes.into(),
                };
            }
            Err(error) if hook.blocking && event_can_deny(event) => {
                return HookRunReport {
                    disposition: HookDisposition::Deny {
                        hook_id: hook.id,
                        message: format!("blocking hook {} failed closed: {error}", hook.name),
                    },
                    notes: notes.into(),
                };
            }
            Err(error) => notes.push(format!("{} failed non-blocking: {error}", hook.name)),
        }
    }
    HookRunReport {
        disposition: HookDisposition::Continue,
        notes: notes.into(),
    }
}

const fn event_can_deny(event: HookEvent) -> bool {
    matches!(event, HookEvent::UserPromptSubmit | HookEvent::PreToolUse)
}

fn bounded_note(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut bounded = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    bounded.push('…');
    bounded
}

#[derive(Debug, Error)]
enum LoadError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("definition exceeds {MAX_DEFINITION_BYTES} bytes")]
    TooLarge,
    #[error("definition is not valid UTF-8")]
    InvalidUtf8,
    #[error("invalid TOML: {0}")]
    InvalidToml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileCommand {
    version: Option<u32>,
    id: Option<String>,
    name: String,
    description: Option<String>,
    argument_hint: Option<String>,
    requires_arguments: Option<bool>,
    prompt: Option<String>,
    prompt_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileHook {
    version: Option<u32>,
    id: Option<String>,
    name: String,
    description: Option<String>,
    event: HookEvent,
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    timeout_ms: Option<u64>,
    max_output_bytes: Option<usize>,
    blocking: Option<bool>,
    enabled: Option<bool>,
    #[serde(default)]
    tool_match: Vec<String>,
}

fn definition_paths(directory: &Path, kind: &str, diagnostics: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            diagnostics.push(format!(
                "could not read {kind} directory {directory:?}: {error}"
            ));
            return Vec::new();
        }
    };
    let mut paths = entries
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(error) => {
                diagnostics.push(format!("could not inspect {kind} directory entry: {error}"));
                None
            }
        })
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "toml")
        })
        .collect::<Vec<_>>();
    paths.sort();
    if paths.len() > MAX_FILES_PER_KIND {
        diagnostics.push(format!(
            "{kind} directory has {} files; only the first {MAX_FILES_PER_KIND} are loaded",
            paths.len()
        ));
        paths.truncate(MAX_FILES_PER_KIND);
    }
    paths
}

fn load_command(
    path: &Path,
    directory: &Path,
    source: AutomationSource,
) -> Result<CustomCommand, LoadError> {
    let raw = read_definition(path)?;
    let command: FileCommand = toml::from_str(&raw)?;
    validate_version(command.version)?;
    let id = definition_id(path, command.id.as_deref())?;
    validate_label("name", &command.name, MAX_NAME_BYTES, false)?;
    let description = command.description.unwrap_or_default();
    validate_label("description", &description, MAX_DESCRIPTION_BYTES, true)?;
    let argument_hint = command.argument_hint.unwrap_or_default();
    validate_label("argument_hint", &argument_hint, 256, true)?;
    let template = load_relative_text(command.prompt, command.prompt_file, directory, "prompt")?;
    if template.trim().is_empty() || template.len() > MAX_TEMPLATE_BYTES {
        return Err(LoadError::Invalid(format!(
            "prompt must be non-blank and at most {MAX_TEMPLATE_BYTES} bytes"
        )));
    }
    Ok(CustomCommand {
        id,
        name: command.name,
        description,
        source,
        source_path: path.to_path_buf(),
        argument_hint,
        requires_arguments: command.requires_arguments.unwrap_or(false),
        template,
    })
}

fn load_hook(path: &Path) -> Result<HookDefinition, LoadError> {
    let raw = read_definition(path)?;
    let hook: FileHook = toml::from_str(&raw)?;
    validate_version(hook.version)?;
    let id = definition_id(path, hook.id.as_deref())?;
    validate_label("name", &hook.name, MAX_NAME_BYTES, false)?;
    let description = hook.description.unwrap_or_default();
    validate_label("description", &description, MAX_DESCRIPTION_BYTES, true)?;
    if !hook.program.is_absolute() {
        return Err(LoadError::Invalid(
            "hook program must be an absolute path".to_owned(),
        ));
    }
    let metadata = fs::symlink_metadata(&hook.program)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LoadError::Invalid(
            "hook program must be a regular, non-symlink file".to_owned(),
        ));
    }
    let program = dunce::canonicalize(&hook.program)?;
    if hook.args.len() > MAX_HOOK_ARGS {
        return Err(LoadError::Invalid(format!(
            "hook has more than {MAX_HOOK_ARGS} arguments"
        )));
    }
    for argument in &hook.args {
        if argument.len() > MAX_HOOK_ARG_BYTES || argument.contains('\0') {
            return Err(LoadError::Invalid(format!(
                "hook argument must be at most {MAX_HOOK_ARG_BYTES} bytes and contain no NUL"
            )));
        }
    }
    let timeout_ms = hook.timeout_ms.unwrap_or(5_000);
    if !(1..=MAX_HOOK_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(LoadError::Invalid(format!(
            "timeout_ms must be between 1 and {MAX_HOOK_TIMEOUT_MS}"
        )));
    }
    let max_output_bytes = hook.max_output_bytes.unwrap_or(16 * 1024);
    if !(1..=MAX_HOOK_OUTPUT_BYTES).contains(&max_output_bytes) {
        return Err(LoadError::Invalid(format!(
            "max_output_bytes must be between 1 and {MAX_HOOK_OUTPUT_BYTES}"
        )));
    }
    let tool_match = validate_tool_match(hook.event, hook.tool_match)?;
    Ok(HookDefinition {
        id,
        name: hook.name,
        description,
        source_path: path.to_path_buf(),
        event: hook.event,
        program,
        args: hook.args.into(),
        timeout: Duration::from_millis(timeout_ms),
        max_output_bytes,
        blocking: hook.blocking.unwrap_or(hook.event == HookEvent::PreToolUse),
        enabled: hook.enabled.unwrap_or(true),
        tool_match: tool_match.into(),
    })
}

fn validate_tool_match(
    event: HookEvent,
    configured: Vec<String>,
) -> Result<Vec<String>, LoadError> {
    let tool_event = matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse);
    if !tool_event && !configured.is_empty() {
        return Err(LoadError::Invalid(
            "tool_match is valid only for pre_tool_use/post_tool_use hooks".to_owned(),
        ));
    }
    let matchers = if tool_event && configured.is_empty() {
        vec!["*".to_owned()]
    } else {
        configured
    };
    let known = [
        "*",
        "read_file",
        "list_directory",
        "search_code",
        "apply_patch",
        "write_file",
        "execute_command",
    ];
    let mut unique = BTreeSet::new();
    for matcher in matchers {
        if !known.contains(&matcher.as_str()) {
            return Err(LoadError::Invalid(format!(
                "unknown tool_match value {matcher:?}"
            )));
        }
        if !unique.insert(matcher.clone()) {
            return Err(LoadError::Invalid(format!(
                "duplicate tool_match value {matcher:?}"
            )));
        }
    }
    Ok(unique.into_iter().collect())
}

fn read_definition(path: &Path) -> Result<String, LoadError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LoadError::Invalid(
            "definition must be a regular, non-symlink file".to_owned(),
        ));
    }
    if metadata.len() > MAX_DEFINITION_BYTES {
        return Err(LoadError::TooLarge);
    }
    String::from_utf8(fs::read(path)?).map_err(|_| LoadError::InvalidUtf8)
}

fn load_relative_text(
    inline: Option<String>,
    file: Option<PathBuf>,
    directory: &Path,
    field: &str,
) -> Result<String, LoadError> {
    match (inline, file) {
        (Some(_), Some(_)) => Err(LoadError::Invalid(format!(
            "use either {field} or {field}_file, not both"
        ))),
        (Some(value), None) => {
            if value.contains('\0') {
                return Err(LoadError::Invalid(format!("{field} contains NUL")));
            }
            Ok(value)
        }
        (None, Some(relative)) => {
            if relative.is_absolute() {
                return Err(LoadError::Invalid(format!(
                    "{field}_file must be relative to its definition directory"
                )));
            }
            let directory = dunce::canonicalize(directory)?;
            let requested = directory.join(relative);
            let metadata = fs::symlink_metadata(&requested)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(LoadError::Invalid(format!(
                    "{field}_file must be a regular, non-symlink file"
                )));
            }
            if metadata.len() > MAX_TEMPLATE_BYTES as u64 {
                return Err(LoadError::Invalid(format!(
                    "{field}_file exceeds {MAX_TEMPLATE_BYTES} bytes"
                )));
            }
            let canonical = dunce::canonicalize(requested)?;
            if !canonical.starts_with(&directory) {
                return Err(LoadError::Invalid(format!(
                    "{field}_file escapes its definition directory"
                )));
            }
            String::from_utf8(fs::read(canonical)?).map_err(|_| LoadError::InvalidUtf8)
        }
        (None, None) => Err(LoadError::Invalid(format!(
            "missing {field} or {field}_file"
        ))),
    }
}

fn validate_version(version: Option<u32>) -> Result<(), LoadError> {
    if version.unwrap_or(FORMAT_VERSION) != FORMAT_VERSION {
        return Err(LoadError::Invalid(format!(
            "unsupported version; expected {FORMAT_VERSION}"
        )));
    }
    Ok(())
}

fn definition_id(path: &Path, configured: Option<&str>) -> Result<String, LoadError> {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| LoadError::Invalid("definition filename must be valid UTF-8".to_owned()))?;
    let id = configured.unwrap_or(stem);
    if id.is_empty()
        || id.len() > MAX_ID_BYTES
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(LoadError::Invalid(
            "id must start with an ASCII letter/digit and contain lowercase letters, digits, '-' or '_'"
                .to_owned(),
        ));
    }
    Ok(id.to_owned())
}

fn validate_label(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), LoadError> {
    if (!allow_empty && value.trim().is_empty()) || value.len() > max_bytes {
        return Err(LoadError::Invalid(format!(
            "{field} must be {} and at most {max_bytes} bytes",
            if allow_empty {
                "valid UTF-8"
            } else {
                "non-blank"
            }
        )));
    }
    if value.chars().any(char::is_control) {
        return Err(LoadError::Invalid(format!(
            "{field} contains control characters"
        )));
    }
    Ok(())
}

fn split_arguments(source: &str) -> Result<Vec<String>, AutomationError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Quote {
        Single,
        Double,
    }
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut started = false;
    for character in source.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        if character == '\\' && quote != Some(Quote::Single) {
            escaped = true;
            started = true;
            continue;
        }
        match (quote, character) {
            (None, '\'') => {
                quote = Some(Quote::Single);
                started = true;
            }
            (None, '"') => {
                quote = Some(Quote::Double);
                started = true;
            }
            (Some(Quote::Single), '\'') => quote = None,
            (Some(Quote::Double), '"') => quote = None,
            (None, character) if character.is_whitespace() => {
                if started {
                    push_argument(&mut arguments, std::mem::take(&mut current))?;
                    started = false;
                }
            }
            _ => {
                current.push(character);
                started = true;
            }
        }
    }
    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        return Err(AutomationError::UnclosedQuote);
    }
    if started {
        push_argument(&mut arguments, current)?;
    }
    Ok(arguments)
}

fn push_argument(arguments: &mut Vec<String>, argument: String) -> Result<(), AutomationError> {
    if arguments.len() >= MAX_ARGUMENTS {
        return Err(AutomationError::TooManyArguments);
    }
    if argument.len() > MAX_ARGUMENT_BYTES {
        return Err(AutomationError::ArgumentTooLarge);
    }
    arguments.push(argument);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, sync::Arc, time::Duration};

    use tempfile::tempdir;
    use tokio_util::sync::CancellationToken;

    use crate::tools::SandboxRoot;

    use super::{
        AutomationCatalog, AutomationError, AutomationSource, CustomCommand, HookDefinition,
        HookDisposition, HookEvent, run_hooks,
    };

    #[test]
    fn command_arguments_are_data_not_recursively_expanded_templates()
    -> Result<(), Box<dyn std::error::Error>> {
        let command = CustomCommand {
            id: "literal".to_owned(),
            name: "Literal".to_owned(),
            description: String::new(),
            source: AutomationSource::Project,
            source_path: PathBuf::from("literal.toml"),
            argument_hint: "<value>".to_owned(),
            requires_arguments: true,
            template: "Value: ${1}".to_owned(),
        };

        assert_eq!(command.expand("'${2}'")?, "Value: ${2}");
        Ok(())
    }

    #[test]
    fn quoted_empty_arguments_keep_their_positional_slot() -> Result<(), Box<dyn std::error::Error>>
    {
        let command = CustomCommand {
            id: "empty".to_owned(),
            name: "Empty".to_owned(),
            description: String::new(),
            source: AutomationSource::Project,
            source_path: PathBuf::from("empty.toml"),
            argument_hint: "<first> <second>".to_owned(),
            requires_arguments: true,
            template: "${1}|${2}".to_owned(),
        };

        assert_eq!(command.expand("\"\" second")?, "|second");
        Ok(())
    }

    #[test]
    fn repeated_placeholders_stop_at_the_expansion_limit() {
        let command = CustomCommand {
            id: "bounded".to_owned(),
            name: "Bounded".to_owned(),
            description: String::new(),
            source: AutomationSource::Project,
            source_path: PathBuf::from("bounded.toml"),
            argument_hint: "<value>".to_owned(),
            requires_arguments: true,
            template: "${1}".repeat(8_000),
        };

        let result = command.expand(&"x".repeat(8_000));
        assert!(matches!(
            result,
            Err(AutomationError::ExpandedCommandTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn exit_two_denies_pre_tool_use_with_the_hook_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let sandbox = SandboxRoot::open(workspace.path())?;
        let (program, args) = denying_program()?;
        let hook = HookDefinition {
            id: "guard".to_owned(),
            name: "Guard".to_owned(),
            description: String::new(),
            source_path: PathBuf::from("guard.toml"),
            event: HookEvent::PreToolUse,
            program,
            args: args.into(),
            timeout: Duration::from_secs(5),
            max_output_bytes: 4_096,
            blocking: true,
            enabled: true,
            tool_match: Arc::from(["write_file".to_owned()]),
        };

        let report = run_hooks(
            vec![hook],
            HookEvent::PreToolUse,
            &serde_json::json!({ "tool": "write_file" }),
            &sandbox,
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(
            report.disposition,
            HookDisposition::Deny {
                ref hook_id,
                ref message,
            } if hook_id == "guard" && message.contains("denied by test")
        ));
        Ok(())
    }

    #[test]
    fn command_expansion_supports_named_and_positional_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let user = tempdir()?;
        let project = workspace.path().join(".decode").join("commands");
        fs::create_dir_all(&project)?;
        fs::write(
            project.join("review.toml"),
            r#"
name = "Review"
description = "Review one area"
argument_hint = "<area> [focus]"
requires_arguments = true
prompt = "Review ${1}. Focus: ${2}. Raw: $ARGUMENTS"
"#,
        )?;
        let catalog = AutomationCatalog::load_from(
            dunce::canonicalize(workspace.path())?,
            Some(user.path().to_path_buf()),
        );
        assert_eq!(
            catalog.expand_invocation("/review \"agent loop\" safety")?,
            Some("Review agent loop. Focus: safety. Raw: \"agent loop\" safety".to_owned())
        );
        assert!(catalog.expand_invocation("/review only-one").is_err());
        Ok(())
    }

    #[test]
    fn user_command_overrides_project_and_plain_arguments_are_appended()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let user = tempdir()?;
        let project = workspace.path().join(".decode").join("commands");
        let user_commands = user.path().join("commands");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&user_commands)?;
        fs::write(project.join("fix.toml"), "name='Project'\nprompt='project'")?;
        fs::write(user_commands.join("fix.toml"), "name='User'\nprompt='user'")?;
        let catalog = AutomationCatalog::load_from(
            dunce::canonicalize(workspace.path())?,
            Some(user.path().to_path_buf()),
        );
        assert_eq!(
            catalog.expand_invocation("/fix parser")?,
            Some("user\n\nArguments: parser".to_owned())
        );
        assert_eq!(catalog.snapshot().commands[0].name, "User");
        Ok(())
    }

    #[test]
    fn project_executable_hooks_are_ignored() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let user = tempdir()?;
        let hooks = workspace.path().join(".decode").join("hooks");
        fs::create_dir_all(&hooks)?;
        fs::write(hooks.join("unsafe.toml"), "name='unsafe'")?;
        let catalog = AutomationCatalog::load_from(
            dunce::canonicalize(workspace.path())?,
            Some(user.path().to_path_buf()),
        );
        assert!(catalog.snapshot().hooks.is_empty());
        assert!(
            catalog
                .snapshot()
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains("ignored"))
        );
        Ok(())
    }

    #[test]
    fn non_tool_hook_cannot_smuggle_tool_match() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let user = tempdir()?;
        let hooks = user.path().join("hooks");
        fs::create_dir_all(&hooks)?;
        let program = user.path().join("hook.exe");
        fs::write(&program, "fixture")?;
        fs::write(
            hooks.join("bad.toml"),
            format!(
                "name='bad'\nevent='session_start'\nprogram={:?}\ntool_match=['write_file']\n",
                program.display().to_string()
            ),
        )?;
        let catalog = AutomationCatalog::load_from(
            dunce::canonicalize(workspace.path())?,
            Some(user.path().to_path_buf()),
        );
        assert!(
            catalog
                .matching_hooks(HookEvent::SessionStart, None)
                .is_empty()
        );
        assert_eq!(catalog.snapshot().diagnostics.len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    fn denying_program() -> Result<(PathBuf, Vec<String>), Box<dyn std::error::Error>> {
        let program = PathBuf::from("/bin/sh");
        if !program.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "/bin/sh is unavailable",
            )
            .into());
        }
        Ok((
            program,
            vec![
                "-c".to_owned(),
                "printf 'denied by test'; exit 2".to_owned(),
            ],
        ))
    }

    #[cfg(windows)]
    fn denying_program() -> Result<(PathBuf, Vec<String>), Box<dyn std::error::Error>> {
        let system_root = std::env::var_os("SystemRoot").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "SystemRoot is unavailable")
        })?;
        let program = PathBuf::from(system_root).join("System32").join("cmd.exe");
        if !program.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{} is unavailable", program.display()),
            )
            .into());
        }
        Ok((
            program,
            vec![
                "/D".to_owned(),
                "/Q".to_owned(),
                "/C".to_owned(),
                "echo denied by test & exit /b 2".to_owned(),
            ],
        ))
    }
}
