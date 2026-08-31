use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_width::UnicodeWidthStr as _;

use crate::api::ReasoningEffort;

use super::subagents::SubagentMode;

const PROFILE_VERSION: u32 = 1;
const MAX_PROFILE_FILES: usize = 64;
const MAX_PROFILE_FILE_BYTES: u64 = 128 * 1024;
const MAX_PROFILE_INSTRUCTIONS_BYTES: u64 = 64 * 1024;
const MAX_PROFILE_NAME_BYTES: usize = 96;
const MAX_PROFILE_DESCRIPTION_BYTES: usize = 1_024;
const MAX_PROFILE_ID_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTool {
    ReadFile,
    ListDirectory,
    SearchCode,
    ApplyPatch,
    WriteFile,
    ExecuteCommand,
}

impl AgentTool {
    pub const ALL: [Self; 6] = [
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
            Self::ReadFile => "read_file",
            Self::ListDirectory => "list_directory",
            Self::SearchCode => "search_code",
            Self::ApplyPatch => "apply_patch",
            Self::WriteFile => "write_file",
            Self::ExecuteCommand => "execute_command",
        }
    }

    #[must_use]
    pub const fn is_mutating(self) -> bool {
        matches!(
            self,
            Self::ApplyPatch | Self::WriteFile | Self::ExecuteCommand
        )
    }
}

impl fmt::Display for AgentTool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentProfileSource {
    BuiltIn,
    User,
    Project,
}

impl fmt::Display for AgentProfileSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::BuiltIn => "built-in",
            Self::User => "user",
            Self::Project => "project",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: AgentProfileSource,
    pub source_path: Option<PathBuf>,
    pub mode: SubagentMode,
    pub deployment: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_iterations: Option<u32>,
    pub allowed_tools: Arc<[AgentTool]>,
    pub instructions: String,
}

impl AgentProfile {
    #[must_use]
    pub fn allows(&self, tool_name: &str) -> bool {
        self.allowed_tools
            .iter()
            .any(|tool| tool.name() == tool_name)
    }

    #[must_use]
    pub fn summary(&self) -> AgentProfileSummary {
        AgentProfileSummary {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            source: self.source,
            source_path: self.source_path.clone(),
            mode: self.mode,
            deployment: self.deployment.clone(),
            reasoning_effort: self.reasoning_effort,
            max_tool_iterations: self.max_tool_iterations,
            allowed_tools: Arc::clone(&self.allowed_tools),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: AgentProfileSource,
    pub source_path: Option<PathBuf>,
    pub mode: SubagentMode,
    pub deployment: Option<String>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tool_iterations: Option<u32>,
    pub allowed_tools: Arc<[AgentTool]>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProfileCatalogSnapshot {
    pub revision: u64,
    pub user_dir: Option<PathBuf>,
    pub project_dir: PathBuf,
    pub profiles: Arc<[AgentProfileSummary]>,
    pub diagnostics: Arc<[String]>,
}

impl Default for AgentProfileCatalogSnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            user_dir: None,
            project_dir: PathBuf::new(),
            profiles: built_in_profiles()
                .iter()
                .map(AgentProfile::summary)
                .collect::<Vec<_>>()
                .into(),
            diagnostics: Arc::from([]),
        }
    }
}

#[derive(Debug, Error)]
pub enum AgentProfileError {
    #[error("unknown agent profile {0:?}")]
    Unknown(String),
}

#[derive(Debug)]
pub struct AgentProfileCatalog {
    workspace_root: PathBuf,
    user_dir: Option<PathBuf>,
    revision: u64,
    profiles: BTreeMap<String, AgentProfile>,
    diagnostics: Vec<String>,
}

impl AgentProfileCatalog {
    #[must_use]
    pub fn load(workspace_root: PathBuf) -> Self {
        let user_dir = directories::ProjectDirs::from("dev", "denysoid", "decode")
            .map(|directories| directories.config_dir().join("agents"));
        let mut catalog = Self {
            workspace_root,
            user_dir,
            revision: 0,
            profiles: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        catalog.reload();
        catalog
    }

    pub fn reload(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.profiles.clear();
        self.diagnostics.clear();
        for profile in built_in_profiles() {
            self.profiles.insert(profile.id.clone(), profile);
        }

        if let Some(user_dir) = self.user_dir.clone() {
            self.load_directory(&user_dir, AgentProfileSource::User);
        }
        let project_dir = self.workspace_root.join(".decode").join("agents");
        self.load_directory(&project_dir, AgentProfileSource::Project);
    }

    #[must_use]
    pub fn snapshot(&self) -> AgentProfileCatalogSnapshot {
        AgentProfileCatalogSnapshot {
            revision: self.revision,
            user_dir: self.user_dir.clone(),
            project_dir: self.workspace_root.join(".decode").join("agents"),
            profiles: self
                .profiles
                .values()
                .map(AgentProfile::summary)
                .collect::<Vec<_>>()
                .into(),
            diagnostics: self.diagnostics.clone().into(),
        }
    }

    pub fn resolve(&self, id: &str) -> Result<AgentProfile, AgentProfileError> {
        self.profiles
            .get(id)
            .cloned()
            .ok_or_else(|| AgentProfileError::Unknown(id.to_owned()))
    }

    fn load_directory(&mut self, directory: &Path, source: AgentProfileSource) {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.diagnostics.push(format!(
                    "could not read {} profile directory {:?}: {error}",
                    source, directory
                ));
                return;
            }
        };
        let mut paths = entries
            .filter_map(|entry| match entry {
                Ok(entry) => Some(entry.path()),
                Err(error) => {
                    self.diagnostics.push(format!(
                        "could not inspect an entry in {} profile directory {:?}: {error}",
                        source, directory
                    ));
                    None
                }
            })
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .collect::<Vec<_>>();
        paths.sort();
        if paths.len() > MAX_PROFILE_FILES {
            self.diagnostics.push(format!(
                "{} profile directory {:?} has {} TOML files; only the first {MAX_PROFILE_FILES} are loaded",
                source,
                directory,
                paths.len()
            ));
            paths.truncate(MAX_PROFILE_FILES);
        }
        for path in paths {
            match load_profile(&path, directory, source) {
                Ok(profile) => {
                    if self.profiles.contains_key(&profile.id) {
                        self.diagnostics.push(format!(
                            "duplicate agent profile id {:?} at {:?}; the first definition remains active",
                            profile.id, path
                        ));
                    } else {
                        self.profiles.insert(profile.id.clone(), profile);
                    }
                }
                Err(error) => self
                    .diagnostics
                    .push(format!("agent profile {:?} was skipped: {error}", path)),
            }
        }
    }
}

#[derive(Debug, Error)]
enum LoadError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("file exceeds {MAX_PROFILE_FILE_BYTES} bytes")]
    TooLarge,
    #[error("file is not valid UTF-8")]
    InvalidUtf8,
    #[error("invalid TOML: {0}")]
    InvalidToml(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileAgentProfile {
    version: Option<u32>,
    id: Option<String>,
    name: String,
    description: Option<String>,
    mode: SubagentMode,
    deployment: Option<String>,
    reasoning_effort: Option<ReasoningEffort>,
    max_tool_iterations: Option<u32>,
    tools: Option<Vec<AgentTool>>,
    instructions: Option<String>,
    instructions_file: Option<PathBuf>,
}

fn load_profile(
    path: &Path,
    directory: &Path,
    source: AgentProfileSource,
) -> Result<AgentProfile, LoadError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(LoadError::Invalid(
            "profile must be a regular, non-symlink file".to_owned(),
        ));
    }
    if metadata.len() > MAX_PROFILE_FILE_BYTES {
        return Err(LoadError::TooLarge);
    }
    let bytes = fs::read(path)?;
    let source_text = String::from_utf8(bytes).map_err(|_| LoadError::InvalidUtf8)?;
    let profile: FileAgentProfile = toml::from_str(&source_text)?;
    if profile.version.unwrap_or(PROFILE_VERSION) != PROFILE_VERSION {
        return Err(LoadError::Invalid(format!(
            "unsupported version; expected {PROFILE_VERSION}"
        )));
    }

    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| LoadError::Invalid("profile filename must be valid UTF-8".to_owned()))?;
    let local_id = profile.id.as_deref().unwrap_or(stem);
    validate_id(local_id)?;
    validate_text("name", &profile.name, MAX_PROFILE_NAME_BYTES, false)?;
    let description = profile.description.unwrap_or_default();
    validate_text(
        "description",
        &description,
        MAX_PROFILE_DESCRIPTION_BYTES,
        true,
    )?;

    if source == AgentProfileSource::Project
        && (profile.deployment.is_some() || profile.reasoning_effort.is_some())
    {
        return Err(LoadError::Invalid(
            "project profiles cannot select deployment or reasoning_effort; move this trusted profile to the user profile directory"
                .to_owned(),
        ));
    }
    if let Some(deployment) = &profile.deployment {
        validate_text("deployment", deployment, 128, false)?;
    }
    if let Some(limit) = profile.max_tool_iterations
        && !(1..=100).contains(&limit)
    {
        return Err(LoadError::Invalid(
            "max_tool_iterations must be between 1 and 100".to_owned(),
        ));
    }

    let allowed_tools = validated_tools(profile.tools, profile.mode, source)?;
    let instructions =
        load_instructions(profile.instructions, profile.instructions_file, directory)?;
    Ok(AgentProfile {
        id: format!("{}:{local_id}", source_prefix(source)),
        name: profile.name,
        description,
        source,
        source_path: Some(path.to_path_buf()),
        mode: profile.mode,
        deployment: profile.deployment,
        reasoning_effort: profile.reasoning_effort,
        max_tool_iterations: profile.max_tool_iterations,
        allowed_tools: allowed_tools.into(),
        instructions,
    })
}

fn validated_tools(
    configured: Option<Vec<AgentTool>>,
    mode: SubagentMode,
    source: AgentProfileSource,
) -> Result<Vec<AgentTool>, LoadError> {
    let defaults = match mode {
        SubagentMode::Research => vec![
            AgentTool::ReadFile,
            AgentTool::ListDirectory,
            AgentTool::SearchCode,
        ],
        SubagentMode::Writer => vec![
            AgentTool::ReadFile,
            AgentTool::ListDirectory,
            AgentTool::SearchCode,
            AgentTool::ApplyPatch,
            AgentTool::WriteFile,
        ],
    };
    let tools = configured.unwrap_or(defaults);
    if tools.is_empty() {
        return Err(LoadError::Invalid(
            "tools must contain at least one capability".to_owned(),
        ));
    }
    let unique = tools.iter().copied().collect::<BTreeSet<_>>();
    if unique.len() != tools.len() {
        return Err(LoadError::Invalid(
            "tools contains duplicate capabilities".to_owned(),
        ));
    }
    if mode == SubagentMode::Research && unique.iter().any(|tool| tool.is_mutating()) {
        return Err(LoadError::Invalid(
            "research profiles cannot request mutating tools".to_owned(),
        ));
    }
    if source == AgentProfileSource::Project && unique.contains(&AgentTool::ExecuteCommand) {
        return Err(LoadError::Invalid(
            "project profiles cannot enable execute_command; use a trusted user profile".to_owned(),
        ));
    }
    Ok(AgentTool::ALL
        .into_iter()
        .filter(|tool| unique.contains(tool))
        .collect())
}

fn load_instructions(
    inline: Option<String>,
    file: Option<PathBuf>,
    profile_directory: &Path,
) -> Result<String, LoadError> {
    match (inline, file) {
        (Some(_), Some(_)) => Err(LoadError::Invalid(
            "use either instructions or instructions_file, not both".to_owned(),
        )),
        (Some(instructions), None) => {
            validate_instruction_text(&instructions)?;
            Ok(instructions)
        }
        (None, Some(relative)) => {
            if relative.is_absolute() {
                return Err(LoadError::Invalid(
                    "instructions_file must be relative to the profile directory".to_owned(),
                ));
            }
            let directory = dunce::canonicalize(profile_directory)?;
            let requested = profile_directory.join(relative);
            let metadata = fs::symlink_metadata(&requested)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(LoadError::Invalid(
                    "instructions_file must be a regular, non-symlink file".to_owned(),
                ));
            }
            if metadata.len() > MAX_PROFILE_INSTRUCTIONS_BYTES {
                return Err(LoadError::Invalid(format!(
                    "instructions_file exceeds {MAX_PROFILE_INSTRUCTIONS_BYTES} bytes"
                )));
            }
            let canonical = dunce::canonicalize(&requested)?;
            if !canonical.starts_with(&directory) {
                return Err(LoadError::Invalid(
                    "instructions_file escapes the profile directory".to_owned(),
                ));
            }
            let instructions =
                String::from_utf8(fs::read(canonical)?).map_err(|_| LoadError::InvalidUtf8)?;
            validate_instruction_text(&instructions)?;
            Ok(instructions)
        }
        (None, None) => Ok(String::new()),
    }
}

fn validate_instruction_text(value: &str) -> Result<(), LoadError> {
    if value.len() as u64 > MAX_PROFILE_INSTRUCTIONS_BYTES {
        return Err(LoadError::Invalid(format!(
            "instructions exceed {MAX_PROFILE_INSTRUCTIONS_BYTES} bytes"
        )));
    }
    if value.contains('\0') {
        return Err(LoadError::Invalid(
            "instructions contain a NUL byte".to_owned(),
        ));
    }
    Ok(())
}

fn validate_text(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), LoadError> {
    if (!allow_empty && (value.trim().is_empty() || value.width() == 0)) || value.len() > max_bytes
    {
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

fn validate_id(id: &str) -> Result<(), LoadError> {
    if id.is_empty()
        || id.len() > MAX_PROFILE_ID_BYTES
        || !id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(LoadError::Invalid(
            "id must start with an ASCII letter/digit and contain only lowercase letters, digits, '-' or '_'"
                .to_owned(),
        ));
    }
    Ok(())
}

const fn source_prefix(source: AgentProfileSource) -> &'static str {
    match source {
        AgentProfileSource::BuiltIn => "builtin",
        AgentProfileSource::User => "user",
        AgentProfileSource::Project => "project",
    }
}

fn built_in_profiles() -> [AgentProfile; 2] {
    [
        AgentProfile {
            id: "builtin:research".to_owned(),
            name: "Research".to_owned(),
            description: "Read-only investigation with filesystem search; cannot edit or run shell commands."
                .to_owned(),
            source: AgentProfileSource::BuiltIn,
            source_path: None,
            mode: SubagentMode::Research,
            deployment: None,
            reasoning_effort: None,
            max_tool_iterations: None,
            allowed_tools: vec![
                AgentTool::ReadFile,
                AgentTool::ListDirectory,
                AgentTool::SearchCode,
            ]
            .into(),
            instructions: "Investigate narrowly, cite concrete files and return evidence rather than guesses."
                .to_owned(),
        },
        AgentProfile {
            id: "builtin:writer".to_owned(),
            name: "Writer".to_owned(),
            description: "Edits an isolated Git worktree. Every resulting file remains pending until user review."
                .to_owned(),
            source: AgentProfileSource::BuiltIn,
            source_path: None,
            mode: SubagentMode::Writer,
            deployment: None,
            reasoning_effort: None,
            max_tool_iterations: None,
            allowed_tools: AgentTool::ALL.to_vec().into(),
            instructions: "Implement the delegated change only, validate it, and report every changed file."
                .to_owned(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{AgentProfileCatalog, AgentProfileSource, AgentTool};

    #[test]
    fn built_in_profiles_are_always_available() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        let research = catalog.resolve("builtin:research")?;
        assert_eq!(research.source, AgentProfileSource::BuiltIn);
        assert!(research.allows("read_file"));
        assert!(!research.allows("write_file"));
        assert!(catalog.resolve("builtin:writer")?.allows("execute_command"));
        Ok(())
    }

    #[test]
    fn project_profile_loads_with_restricted_capabilities() -> Result<(), Box<dyn std::error::Error>>
    {
        let workspace = tempdir()?;
        let directory = workspace.path().join(".decode").join("agents");
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("docs.toml"),
            r#"
version = 1
name = "Docs reviewer"
description = "Checks public documentation"
mode = "research"
tools = ["read_file", "search_code"]
instructions = "Check examples against the implementation."
"#,
        )?;
        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        let profile = catalog.resolve("project:docs")?;
        assert!(profile.allows("read_file"));
        assert!(!profile.allows("list_directory"));
        assert_eq!(
            profile.allowed_tools.as_ref(),
            &[AgentTool::ReadFile, AgentTool::SearchCode]
        );
        assert!(catalog.snapshot().diagnostics.is_empty());
        Ok(())
    }

    #[test]
    fn project_profile_cannot_enable_shell_or_runtime_overrides()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let directory = workspace.path().join(".decode").join("agents");
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("unsafe.toml"),
            r#"
name = "Unsafe"
mode = "writer"
deployment = "expensive-model"
tools = ["read_file", "execute_command"]
"#,
        )?;
        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        assert!(catalog.resolve("project:unsafe").is_err());
        assert_eq!(catalog.snapshot().diagnostics.len(), 1);
        Ok(())
    }

    #[test]
    fn instructions_file_cannot_escape_profile_directory() -> Result<(), Box<dyn std::error::Error>>
    {
        let workspace = tempdir()?;
        let directory = workspace.path().join(".decode").join("agents");
        fs::create_dir_all(&directory)?;
        fs::write(workspace.path().join("outside.md"), "secret")?;
        fs::write(
            directory.join("escape.toml"),
            r#"
name = "Escape"
mode = "research"
instructions_file = "../../outside.md"
"#,
        )?;
        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        assert!(catalog.resolve("project:escape").is_err());
        assert!(catalog.snapshot().diagnostics[0].contains("escapes"));
        Ok(())
    }

    #[test]
    fn torn_profile_is_diagnostic_and_does_not_hide_built_ins()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let directory = workspace.path().join(".decode").join("agents");
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("torn.toml"), "name = \"unfinished")?;

        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        assert!(catalog.resolve("project:torn").is_err());
        assert!(catalog.resolve("builtin:research").is_ok());
        assert_eq!(catalog.snapshot().diagnostics.len(), 1);
        Ok(())
    }

    #[test]
    fn profile_name_must_have_a_visible_glyph() -> Result<(), Box<dyn std::error::Error>> {
        let workspace = tempdir()?;
        let directory = workspace.path().join(".decode").join("agents");
        fs::create_dir_all(&directory)?;
        fs::write(
            directory.join("invisible.toml"),
            "name = \"\u{200b}\u{2060}\"\nmode = \"research\"\n",
        )?;

        let catalog = AgentProfileCatalog::load(dunce::canonicalize(workspace.path())?);
        assert!(catalog.resolve("project:invisible").is_err());
        assert_eq!(catalog.snapshot().diagnostics.len(), 1);
        Ok(())
    }
}
