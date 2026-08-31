use std::{
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
};
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const PROJECT_RULES_FILE: &str = ".decodeignore";
pub const USER_RULES_FILE: &str = "privacy.ignore";
const MAX_RULE_FILE_BYTES: u64 = 128 * 1024;
const MAX_RULES_PER_SOURCE: usize = 1_024;

const BUILTIN_RULES: &[&str] = &[
    ".env",
    ".env.*",
    "!.env.example",
    "!.env.sample",
    "!.env.template",
    "*.pem",
    "*.p12",
    "*.pfx",
    "*.jks",
    "id_rsa",
    "id_rsa.*",
    "id_ed25519",
    "id_ed25519.*",
    ".ssh",
    ".ssh/**",
    ".aws/credentials",
    ".aws/config",
    ".azure",
    ".azure/**",
    ".kube/config",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "service-account*.json",
    "credentials*.json",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacySourceSnapshot {
    pub id: &'static str,
    pub label: &'static str,
    pub location: String,
    pub active: bool,
    pub fail_closed: bool,
    pub rule_count: usize,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacySnapshot {
    pub revision: u64,
    pub policy_sha256: String,
    pub blocked_attempts: u64,
    pub sources: Arc<[PrivacySourceSnapshot]>,
}

impl Default for PrivacySnapshot {
    fn default() -> Self {
        Self {
            revision: 0,
            policy_sha256: String::new(),
            blocked_attempts: 0,
            sources: Arc::from([]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PrivacyError {
    #[error("privacy policy root {path:?} could not be canonicalized: {message}")]
    InvalidRoot { path: PathBuf, message: String },
    #[error("built-in privacy policy could not be compiled: {0}")]
    InvalidBuiltins(#[source] PrivacyRuleError),
    #[error("privacy policy state is unavailable; access was denied")]
    Unavailable,
    #[error("sensitive path {path:?} is blocked by {rule_source}")]
    SensitivePath { path: String, rule_source: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PrivacyRuleError {
    #[error("invalid rule on active line {line}: {message}")]
    InvalidRule { line: usize, message: String },
    #[error("rules could not be compiled: {message}")]
    Compile { message: String },
}

#[derive(Debug)]
struct CompiledSource {
    snapshot: PrivacySourceSnapshot,
    matcher: Option<Gitignore>,
    deny_all: bool,
    digest_material: String,
}

impl CompiledSource {
    fn blocks(&self, path: &Path, is_directory: bool) -> bool {
        if self.deny_all {
            return true;
        }
        self.matcher.as_ref().is_some_and(|matcher| {
            matches!(
                matcher.matched_path_or_any_parents(path, is_directory),
                Match::Ignore(_)
            )
        })
    }
}

#[derive(Debug)]
struct CompiledPolicy {
    revision: u64,
    sha256: String,
    sources: Vec<CompiledSource>,
}

#[derive(Debug, Clone)]
pub struct PrivacyShield {
    root: Arc<PathBuf>,
    user_rules_file: Arc<Option<PathBuf>>,
    policy: Arc<RwLock<CompiledPolicy>>,
    reload_lock: Arc<Mutex<()>>,
    blocked_attempts: Arc<AtomicU64>,
}

impl PrivacyShield {
    pub fn load(
        workspace_root: &Path,
        user_rules_file: Option<PathBuf>,
    ) -> Result<Self, PrivacyError> {
        let root =
            dunce::canonicalize(workspace_root).map_err(|error| PrivacyError::InvalidRoot {
                path: workspace_root.to_path_buf(),
                message: error.to_string(),
            })?;
        let policy = compile_policy(&root, user_rules_file.as_deref(), 1)?;
        Ok(Self {
            root: Arc::new(root),
            user_rules_file: Arc::new(user_rules_file),
            policy: Arc::new(RwLock::new(policy)),
            reload_lock: Arc::new(Mutex::new(())),
            blocked_attempts: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn load_project_only(workspace_root: &Path) -> Result<Self, PrivacyError> {
        Self::load(workspace_root, None)
    }

    pub fn reload(&self) -> Result<PrivacySnapshot, PrivacyError> {
        let _reload = self
            .reload_lock
            .lock()
            .map_err(|_| PrivacyError::Unavailable)?;
        let next_revision = self
            .policy
            .read()
            .map_err(|_| PrivacyError::Unavailable)?
            .revision
            .saturating_add(1);
        let replacement = compile_policy(
            &self.root,
            self.user_rules_file.as_ref().as_deref(),
            next_revision,
        )?;
        let mut policy = self.policy.write().map_err(|_| PrivacyError::Unavailable)?;
        *policy = replacement;
        drop(policy);
        self.snapshot()
    }

    pub fn check_relative(&self, path: &Path, is_directory: bool) -> Result<(), PrivacyError> {
        let normalized = match normalize_relative(path) {
            Some(normalized) => normalized,
            None => {
                self.blocked_attempts.fetch_add(1, Ordering::Relaxed);
                return Err(PrivacyError::SensitivePath {
                    path: path.to_string_lossy().into_owned(),
                    rule_source: "invalid path safety policy".to_owned(),
                });
            }
        };
        let policy = self.policy.read().map_err(|_| PrivacyError::Unavailable)?;
        if let Some(source) = blocking_source(&policy, &normalized, is_directory) {
            self.blocked_attempts.fetch_add(1, Ordering::Relaxed);
            return Err(PrivacyError::SensitivePath {
                path: normalized.to_string_lossy().replace('\\', "/"),
                rule_source: source.snapshot.label.to_owned(),
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn allows_relative(&self, path: &Path, is_directory: bool) -> bool {
        let Some(normalized) = normalize_relative(path) else {
            return false;
        };
        self.policy
            .read()
            .map(|policy| blocking_source(&policy, &normalized, is_directory).is_none())
            .unwrap_or(false)
    }

    pub fn snapshot(&self) -> Result<PrivacySnapshot, PrivacyError> {
        let policy = self.policy.read().map_err(|_| PrivacyError::Unavailable)?;
        Ok(PrivacySnapshot {
            revision: policy.revision,
            policy_sha256: policy.sha256.clone(),
            blocked_attempts: self.blocked_attempts.load(Ordering::Relaxed),
            sources: Arc::from(
                policy
                    .sources
                    .iter()
                    .map(|source| source.snapshot.clone())
                    .collect::<Vec<_>>(),
            ),
        })
    }

    pub fn policy_sha256(&self) -> Result<String, PrivacyError> {
        self.policy
            .read()
            .map(|policy| policy.sha256.clone())
            .map_err(|_| PrivacyError::Unavailable)
    }
}

fn blocking_source<'a>(
    policy: &'a CompiledPolicy,
    path: &Path,
    is_directory: bool,
) -> Option<&'a CompiledSource> {
    policy
        .sources
        .iter()
        .find(|source| source.blocks(path, is_directory))
}

fn compile_policy(
    root: &Path,
    user_rules_file: Option<&Path>,
    revision: u64,
) -> Result<CompiledPolicy, PrivacyError> {
    let mut sources = Vec::with_capacity(3);
    sources.push(compile_builtin_source()?);
    sources.push(compile_file_source(
        "user",
        "User privacy rules",
        user_rules_file,
    ));
    let project_file = root.join(PROJECT_RULES_FILE);
    sources.push(compile_file_source(
        "project",
        "Project privacy rules",
        Some(&project_file),
    ));

    let mut digest = Sha256::new();
    for source in &sources {
        digest.update(source.snapshot.id.as_bytes());
        digest.update([u8::from(source.snapshot.active)]);
        digest.update([u8::from(source.snapshot.fail_closed)]);
        digest.update(source.snapshot.rule_count.to_le_bytes());
        digest.update(source.snapshot.detail.as_bytes());
        digest.update(source.digest_material.as_bytes());
    }

    Ok(CompiledPolicy {
        revision,
        sha256: format!("{:x}", digest.finalize()),
        sources,
    })
}

fn compile_builtin_source() -> Result<CompiledSource, PrivacyError> {
    let matcher =
        compile_lines(BUILTIN_RULES.iter().copied()).map_err(PrivacyError::InvalidBuiltins)?;
    Ok(CompiledSource {
        snapshot: PrivacySourceSnapshot {
            id: "built-in",
            label: "Built-in secret patterns",
            location: "compiled into DEcode by denysoid".to_owned(),
            active: true,
            fail_closed: false,
            rule_count: BUILTIN_RULES.len(),
            detail: "always active; custom sources cannot weaken these rules".to_owned(),
        },
        matcher: Some(matcher),
        deny_all: false,
        digest_material: BUILTIN_RULES.join("\n"),
    })
}

fn compile_file_source(
    id: &'static str,
    label: &'static str,
    path: Option<&Path>,
) -> CompiledSource {
    let Some(path) = path else {
        return inactive_source(id, label, "not configured".to_owned());
    };
    let location = path.to_string_lossy().into_owned();
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return inactive_source(id, label, location);
        }
        Err(error) => {
            return failed_source(id, label, location, format!("metadata failed: {error}"));
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return failed_source(
            id,
            label,
            location,
            "must be a regular non-symlink file".to_owned(),
        );
    }
    if metadata.len() > MAX_RULE_FILE_BYTES {
        return failed_source(
            id,
            label,
            location,
            format!("exceeds the {MAX_RULE_FILE_BYTES} byte limit"),
        );
    }
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return failed_source(id, label, location, format!("read failed: {error}"));
        }
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            return failed_source(id, label, location, "is not valid UTF-8".to_owned());
        }
    };
    if text.contains('\0') {
        return failed_source(id, label, location, "contains a NUL byte".to_owned());
    }
    let lines = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = if index == 0 {
                line.trim_start_matches('\u{feff}')
            } else {
                line
            };
            let trimmed = line.trim();
            (!trimmed.is_empty() && !trimmed.starts_with('#')).then_some(line)
        })
        .collect::<Vec<_>>();
    if lines.len() > MAX_RULES_PER_SOURCE {
        return failed_source(
            id,
            label,
            location,
            format!("contains more than {MAX_RULES_PER_SOURCE} active rules"),
        );
    }
    let matcher = match compile_lines(lines.iter().copied()) {
        Ok(matcher) => matcher,
        Err(error) => return failed_source(id, label, location, error.to_string()),
    };
    CompiledSource {
        snapshot: PrivacySourceSnapshot {
            id,
            label,
            location,
            active: true,
            fail_closed: false,
            rule_count: lines.len(),
            detail: if lines.is_empty() {
                "loaded; contains no active rules".to_owned()
            } else {
                format!("loaded {} active rule(s)", lines.len())
            },
        },
        matcher: Some(matcher),
        deny_all: false,
        digest_material: text.to_owned(),
    }
}

fn compile_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Result<Gitignore, PrivacyRuleError> {
    let mut builder = GitignoreBuilder::new(Path::new(""));
    #[cfg(windows)]
    builder
        .case_insensitive(true)
        .map_err(|error| PrivacyRuleError::Compile {
            message: error.to_string(),
        })?;
    for (index, line) in lines.enumerate() {
        builder
            .add_line(None, line)
            .map_err(|error| PrivacyRuleError::InvalidRule {
                line: index + 1,
                message: error.to_string(),
            })?;
    }
    builder.build().map_err(|error| PrivacyRuleError::Compile {
        message: error.to_string(),
    })
}

fn inactive_source(id: &'static str, label: &'static str, location: String) -> CompiledSource {
    CompiledSource {
        snapshot: PrivacySourceSnapshot {
            id,
            label,
            location,
            active: false,
            fail_closed: false,
            rule_count: 0,
            detail: "file is absent; source contributes no additional rules".to_owned(),
        },
        matcher: None,
        deny_all: false,
        digest_material: String::new(),
    }
}

fn failed_source(
    id: &'static str,
    label: &'static str,
    location: String,
    detail: String,
) -> CompiledSource {
    CompiledSource {
        snapshot: PrivacySourceSnapshot {
            id,
            label,
            location,
            active: true,
            fail_closed: true,
            rule_count: 0,
            detail: format!("invalid source; all workspace paths denied: {detail}"),
        },
        matcher: None,
        deny_all: true,
        digest_material: detail,
    }
}

fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => normalized.push(value),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, Barrier},
        thread,
    };

    use tempfile::tempdir;

    use super::{PROJECT_RULES_FILE, PrivacyError, PrivacyShield};

    #[test]
    fn builtins_block_nested_secrets_but_allow_templates() -> Result<(), PrivacyError> {
        let root = tempdir().map_err(|error| PrivacyError::InvalidRoot {
            path: std::env::temp_dir(),
            message: error.to_string(),
        })?;
        let shield = PrivacyShield::load_project_only(root.path())?;
        assert!(
            shield
                .check_relative("services/api/.env".as_ref(), false)
                .is_err()
        );
        assert!(
            shield
                .check_relative("services/api/.env.example".as_ref(), false)
                .is_ok()
        );
        assert!(shield.check_relative("src/main.rs".as_ref(), false).is_ok());
        Ok(())
    }

    #[test]
    fn project_rules_are_additive_and_negation_cannot_weaken_builtins()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        fs::write(root.path().join(PROJECT_RULES_FILE), "private/**\n!.env\n")?;
        let shield = PrivacyShield::load_project_only(root.path())?;
        assert!(
            shield
                .check_relative("private/notes.txt".as_ref(), false)
                .is_err()
        );
        assert!(shield.check_relative(".env".as_ref(), false).is_err());
        Ok(())
    }

    #[test]
    fn malformed_source_fails_closed_and_reload_preserves_shared_handle()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let rules = root.path().join(PROJECT_RULES_FILE);
        fs::write(&rules, "private/**\n")?;
        let shield = PrivacyShield::load_project_only(root.path())?;
        let clone = shield.clone();
        fs::write(&rules, b"private\0notes\n")?;
        let snapshot = shield.reload()?;
        assert!(snapshot.sources.iter().any(|source| source.fail_closed));
        assert!(clone.check_relative("src/lib.rs".as_ref(), false).is_err());
        Ok(())
    }

    #[test]
    fn fail_closed_policy_rejects_the_workspace_root() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        fs::write(root.path().join(PROJECT_RULES_FILE), b"private\0notes\n")?;
        let shield = PrivacyShield::load_project_only(root.path())?;

        assert!(shield.check_relative("".as_ref(), true).is_err());
        assert!(!shield.allows_relative("".as_ref(), true));
        Ok(())
    }

    #[test]
    fn rejected_invalid_paths_are_counted() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let shield = PrivacyShield::load_project_only(root.path())?;

        assert!(shield.check_relative("../outside".as_ref(), false).is_err());
        assert_eq!(shield.snapshot()?.blocked_attempts, 1);
        Ok(())
    }

    #[test]
    fn concurrent_reloads_each_advance_the_revision() -> Result<(), Box<dyn std::error::Error>> {
        const RELOADS: usize = 32;

        let root = tempdir()?;
        let shield = PrivacyShield::load_project_only(root.path())?;
        let start = Arc::new(Barrier::new(RELOADS + 1));
        let mut workers = Vec::with_capacity(RELOADS);
        for _ in 0..RELOADS {
            let shield = shield.clone();
            let start = Arc::clone(&start);
            workers.push(thread::spawn(move || {
                start.wait();
                shield.reload()
            }));
        }
        start.wait();
        for worker in workers {
            let result = worker.join().map_err(|_| "reload worker panicked")?;
            result?;
        }

        assert_eq!(shield.snapshot()?.revision, 1 + RELOADS as u64);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_privacy_rules_follow_filesystem_case_semantics()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let shield = PrivacyShield::load_project_only(root.path())?;

        assert!(
            shield
                .check_relative("SERVICE/.ENV".as_ref(), false)
                .is_err()
        );
        Ok(())
    }
}
