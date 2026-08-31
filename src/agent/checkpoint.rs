use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use tokio::{process::Command, time::timeout};
use tokio_util::sync::CancellationToken;

use crate::tools::{SandboxError, SandboxRoot, sandbox::CheckpointRestore};

use super::state::AgentState;

const CHECKPOINT_FORMAT_VERSION: u32 = 3;
const MAX_CHECKPOINTS: usize = 20;
const MAX_CHANGED_PATHS: usize = 5_000;
const MAX_RESTORED_FILE_BYTES: usize = 32 * 1024 * 1024;
const MAX_CHECKPOINT_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
const MAX_GIT_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("checkpoint I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Git operation `{operation}` timed out after {seconds}s")]
    GitTimeout { operation: String, seconds: u64 },
    #[error("Git operation `{operation}` failed: {message}")]
    Git { operation: String, message: String },
    #[error("Git operation `{operation}` returned more than {limit_bytes} bytes")]
    GitOutputTooLarge {
        operation: String,
        limit_bytes: usize,
    },
    #[error("checkpoint journal contains invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("checkpoint sandbox rejected a restore: {0}")]
    Sandbox(#[from] SandboxError),
    #[error("checkpoint {0} does not exist")]
    NotFound(u64),
    #[error("checkpoint changed more than {limit} paths; snapshot was not retained")]
    TooManyPaths { limit: usize },
    #[error("checkpoint payload is larger than the {limit_bytes}-byte retention limit")]
    PayloadTooLarge { limit_bytes: usize },
    #[error("Git returned malformed checkpoint data: {0}")]
    InvalidGitData(String),
    #[error("checkpoint file tracking became incomplete: {0}")]
    TrackingIncomplete(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub id: u64,
    pub created_at: DateTime<Utc>,
    pub prompt_preview: String,
    pub changed_paths: Vec<String>,
    pub history_entries_before: usize,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointConflict {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindReport {
    pub checkpoint_id: u64,
    pub restored_files: Vec<String>,
    pub preserved_conflicts: Vec<CheckpointConflict>,
    pub discarded_checkpoints: usize,
    pub restored_history_entries: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RewindResult {
    pub report: RewindReport,
    pub state_before: AgentState,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingCheckpoint {
    id: u64,
    created_at: DateTime<Utc>,
    prompt: String,
    state_before: AgentState,
    segments: Vec<CheckpointSegment>,
    payload_bytes: usize,
    invalid_reason: Option<String>,
    session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointSegment {
    before_tree: String,
    after_tree: String,
    changed_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckpointRecord {
    version: u32,
    id: u64,
    created_at: DateTime<Utc>,
    prompt: String,
    changed_paths: Vec<String>,
    segments: Vec<CheckpointSegment>,
    state_before: AgentState,
    #[serde(default)]
    session_id: Option<String>,
}

impl CheckpointRecord {
    fn summary(&self) -> CheckpointSummary {
        CheckpointSummary {
            id: self.id,
            created_at: self.created_at,
            prompt_preview: preview_prompt(&self.prompt),
            changed_paths: self.changed_paths.clone(),
            history_entries_before: self.state_before.history.len(),
            session_id: self.session_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct GitTreeEntry {
    mode: String,
    object_id: String,
}

impl GitTreeEntry {
    fn is_executable(&self) -> bool {
        self.mode == "100755"
    }
}

#[derive(Debug)]
struct RestorePlan {
    path: String,
    expected: Option<Vec<u8>>,
    expected_executable: Option<bool>,
    desired: Option<Vec<u8>>,
    desired_executable: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct CheckpointStore {
    root: PathBuf,
    data_dir: PathBuf,
    snapshot_git_dir: PathBuf,
    journal_path: PathBuf,
    sandbox: SandboxRoot,
    git_timeout: Duration,
    records: VecDeque<CheckpointRecord>,
    next_id: u64,
    active_session: Option<String>,
}

impl CheckpointStore {
    pub(crate) fn open(
        workspace_root: &Path,
        git_timeout: Duration,
    ) -> Result<Option<Self>, CheckpointError> {
        let root = fs::canonicalize(workspace_root).map_err(|source| CheckpointError::Io {
            path: workspace_root.to_path_buf(),
            source,
        })?;
        let Some(git_dir) = resolve_git_dir(&root)? else {
            return Ok(None);
        };

        let data_dir = git_dir.join("decode");
        fs::create_dir_all(&data_dir).map_err(|source| CheckpointError::Io {
            path: data_dir.clone(),
            source,
        })?;
        let journal_path = data_dir.join("checkpoints.jsonl");
        let snapshot_git_dir = data_dir.join("snapshot.git");
        let records = if snapshot_git_dir.join("HEAD").is_file() {
            load_records(&journal_path)?
        } else {
            VecDeque::new()
        };
        let next_id = records
            .iter()
            .map(|record| record.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        Ok(Some(Self {
            sandbox: SandboxRoot::open(&root)?,
            root,
            data_dir,
            snapshot_git_dir,
            journal_path,
            git_timeout,
            records,
            next_id,
            active_session: None,
        }))
    }

    #[must_use]
    pub(crate) fn summaries(&self) -> Vec<CheckpointSummary> {
        self.records
            .iter()
            .rev()
            .filter(|record| record.session_id == self.active_session)
            .map(CheckpointRecord::summary)
            .collect()
    }

    pub(crate) fn set_active_session(&mut self, session_id: Option<String>) {
        self.active_session = session_id;
    }

    pub(crate) async fn begin(
        &mut self,
        prompt: &str,
        state_before: &AgentState,
        session_id: Option<String>,
    ) -> Result<PendingCheckpoint, CheckpointError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        Ok(PendingCheckpoint {
            id,
            created_at: Utc::now(),
            prompt: truncate_utf8(prompt, MAX_PROMPT_BYTES),
            state_before: state_before.clone(),
            segments: Vec::new(),
            payload_bytes: 0,
            invalid_reason: None,
            session_id,
        })
    }

    pub(crate) async fn begin_tool_segment(&self) -> Result<String, CheckpointError> {
        self.capture_tree().await
    }

    pub(crate) async fn finish_tool_segment(
        &self,
        pending: &mut PendingCheckpoint,
        before_tree: String,
    ) -> Result<(), CheckpointError> {
        let after_tree = self.capture_tree().await?;
        let changed_paths = self.changed_paths(&before_tree, &after_tree).await?;
        if changed_paths.is_empty() {
            return Ok(());
        }
        if changed_paths.len() > MAX_CHANGED_PATHS {
            return Err(CheckpointError::TooManyPaths {
                limit: MAX_CHANGED_PATHS,
            });
        }
        let segment_bytes = self
            .payload_size(&before_tree, &after_tree, &changed_paths)
            .await?;
        pending.payload_bytes = pending.payload_bytes.saturating_add(segment_bytes);
        if pending.payload_bytes > MAX_CHECKPOINT_PAYLOAD_BYTES {
            return Err(CheckpointError::PayloadTooLarge {
                limit_bytes: MAX_CHECKPOINT_PAYLOAD_BYTES,
            });
        }
        pending.segments.push(CheckpointSegment {
            before_tree,
            after_tree,
            changed_paths,
        });
        Ok(())
    }

    pub(crate) fn invalidate(pending: &mut PendingCheckpoint, reason: String) {
        pending.invalid_reason = Some(reason);
    }

    pub(crate) async fn commit(
        &mut self,
        pending: PendingCheckpoint,
    ) -> Result<CheckpointSummary, CheckpointError> {
        if let Some(reason) = pending.invalid_reason.as_ref() {
            let _ = self.run_git(&["gc", "--prune=now", "--quiet"], None).await;
            return Err(CheckpointError::TrackingIncomplete(reason.clone()));
        }
        let mut changed_paths = pending
            .segments
            .iter()
            .flat_map(|segment| segment.changed_paths.iter().cloned())
            .collect::<Vec<_>>();
        changed_paths.sort();
        changed_paths.dedup();
        if changed_paths.len() > MAX_CHANGED_PATHS {
            return Err(CheckpointError::TooManyPaths {
                limit: MAX_CHANGED_PATHS,
            });
        }
        let checkpoint_id = pending.id;
        if let Err(error) = self.retain_segments(checkpoint_id, &pending.segments).await {
            self.release_segments(checkpoint_id, &pending.segments)
                .await;
            return Err(error);
        }
        let record = CheckpointRecord {
            version: CHECKPOINT_FORMAT_VERSION,
            id: pending.id,
            created_at: pending.created_at,
            prompt: pending.prompt,
            changed_paths,
            segments: pending.segments,
            state_before: pending.state_before,
            session_id: pending.session_id,
        };
        let summary = record.summary();
        self.records.push_back(record);
        let mut expired = VecDeque::new();
        while self.records.len() > MAX_CHECKPOINTS {
            if let Some(record) = self.records.pop_front() {
                expired.push_back(record);
            }
        }
        if let Err(error) = self.persist_records() {
            let new_record = self.records.pop_back();
            while let Some(record) = expired.pop_back() {
                self.records.push_front(record);
            }
            if let Some(record) = new_record {
                self.release_segments(checkpoint_id, &record.segments).await;
            }
            return Err(error);
        }
        let had_expired = !expired.is_empty();
        for record in expired {
            self.release_segments(record.id, &record.segments).await;
        }
        if had_expired
            && let Err(error) = self.run_git(&["gc", "--prune=now", "--quiet"], None).await
        {
            tracing::warn!(%error, "could not compact private checkpoint object store");
        }
        Ok(summary)
    }

    pub(crate) async fn rewind(&mut self, id: u64) -> Result<RewindResult, CheckpointError> {
        let index = self
            .records
            .iter()
            .position(|record| record.id == id)
            .filter(|index| self.records[*index].session_id == self.active_session)
            .ok_or(CheckpointError::NotFound(id))?;
        let record = self
            .records
            .get(index)
            .cloned()
            .ok_or(CheckpointError::NotFound(id))?;

        let mut conflicts = Vec::new();
        let cancel = CancellationToken::new();
        let mut restored_files = Vec::new();
        let mut conflicted_paths = BTreeSet::new();
        for segment in record.segments.iter().rev() {
            for path in &segment.changed_paths {
                if conflicted_paths.contains(path) {
                    continue;
                }
                let plan = match self.restore_plan(segment, path).await {
                    Ok(plan) => plan,
                    Err(error) => {
                        conflicted_paths.insert(path.clone());
                        conflicts.push(CheckpointConflict {
                            path: path.clone(),
                            reason: error.to_string(),
                        });
                        continue;
                    }
                };
                match self.sandbox.checkpoint_compare_and_restore(
                    Path::new(&plan.path),
                    CheckpointRestore {
                        expected_content: plan.expected.as_deref(),
                        expected_executable: plan.expected_executable,
                        desired_content: plan.desired.as_deref(),
                        desired_executable: plan.desired_executable,
                        limit_bytes: MAX_RESTORED_FILE_BYTES,
                    },
                    &cancel,
                ) {
                    Ok(()) => restored_files.push(plan.path),
                    Err(error) => {
                        conflicted_paths.insert(plan.path.clone());
                        conflicts.push(CheckpointConflict {
                            path: plan.path,
                            reason: error.to_string(),
                        });
                    }
                }
            }
        }
        restored_files.sort();
        restored_files.dedup();

        let previous_records = self.records.clone();
        let mut retained = VecDeque::with_capacity(self.records.len());
        let mut removed = VecDeque::new();
        for (record_index, candidate) in self.records.drain(..).enumerate() {
            if record_index >= index && candidate.session_id == self.active_session {
                removed.push_back(candidate);
            } else {
                retained.push_back(candidate);
            }
        }
        self.records = retained;
        let mut discarded = 0;
        match self.persist_records() {
            Ok(()) => {
                discarded = removed.len();
                for discarded_record in removed {
                    self.release_segments(discarded_record.id, &discarded_record.segments)
                        .await;
                }
            }
            Err(error) => {
                self.records = previous_records;
                tracing::error!(%error, "rewind was applied but checkpoint journal could not be updated");
                conflicts.push(CheckpointConflict {
                    path: "[checkpoint journal]".to_owned(),
                    reason: format!(
                        "rewind was applied safely, but retention metadata could not be saved: {error}"
                    ),
                });
            }
        }

        Ok(RewindResult {
            report: RewindReport {
                checkpoint_id: id,
                restored_files,
                preserved_conflicts: conflicts,
                discarded_checkpoints: discarded,
                restored_history_entries: record.state_before.history.len(),
            },
            state_before: record.state_before,
        })
    }

    async fn restore_plan(
        &self,
        segment: &CheckpointSegment,
        path: &str,
    ) -> Result<RestorePlan, CheckpointError> {
        let before = self.tree_entry(&segment.before_tree, path).await?;
        let after = self.tree_entry(&segment.after_tree, path).await?;
        ensure_regular_blob(before.as_ref(), path)?;
        ensure_regular_blob(after.as_ref(), path)?;
        let desired = match before {
            Some(ref entry) => Some(self.read_blob(&entry.object_id).await?),
            None => None,
        };
        let expected = match after {
            Some(ref entry) => Some(self.read_blob(&entry.object_id).await?),
            None => None,
        };
        Ok(RestorePlan {
            path: path.to_owned(),
            expected,
            expected_executable: after.as_ref().map(GitTreeEntry::is_executable),
            desired,
            desired_executable: before.as_ref().map(GitTreeEntry::is_executable),
        })
    }

    async fn capture_tree(&self) -> Result<String, CheckpointError> {
        self.ensure_repository().await?;
        let index_path = std::env::temp_dir().join(format!(
            "decode-checkpoint-{}-{}-{}.idx",
            std::process::id(),
            Utc::now().timestamp_micros(),
            fastrand::u64(..)
        ));
        let result = async {
            self.run_git(&["read-tree", "--empty"], Some(&index_path))
                .await?;
            self.run_git(&["add", "-A", "--", "."], Some(&index_path))
                .await?;
            let output = self.run_git(&["write-tree"], Some(&index_path)).await?;
            let tree = parse_object_id(&output)?;
            Ok(tree)
        }
        .await;
        if let Err(source) = tokio::fs::remove_file(&index_path).await
            && source.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path = ?index_path, %source, "could not remove temporary Git index");
        }
        result
    }

    async fn changed_paths(
        &self,
        before_tree: &str,
        after_tree: &str,
    ) -> Result<Vec<String>, CheckpointError> {
        let output = self
            .run_git(
                &[
                    "diff-tree",
                    "--no-commit-id",
                    "--name-status",
                    "--no-renames",
                    "-r",
                    "-z",
                    before_tree,
                    after_tree,
                ],
                None,
            )
            .await?;
        parse_changed_paths(&output)
    }

    async fn tree_entry(
        &self,
        tree: &str,
        path: &str,
    ) -> Result<Option<GitTreeEntry>, CheckpointError> {
        let output = self
            .run_git(&["ls-tree", "-z", tree, "--", path], None)
            .await?;
        parse_tree_entry(&output, path)
    }

    async fn read_blob(&self, object_id: &str) -> Result<Vec<u8>, CheckpointError> {
        let size = self.blob_size(object_id).await?;
        if size > MAX_RESTORED_FILE_BYTES {
            return Err(CheckpointError::GitOutputTooLarge {
                operation: "cat-file blob".to_owned(),
                limit_bytes: MAX_RESTORED_FILE_BYTES,
            });
        }
        self.run_git_with_limit(
            &["cat-file", "blob", object_id],
            None,
            MAX_RESTORED_FILE_BYTES,
        )
        .await
    }

    async fn blob_size(&self, object_id: &str) -> Result<usize, CheckpointError> {
        let size_output = self.run_git(&["cat-file", "-s", object_id], None).await?;
        let size_text = String::from_utf8(size_output)
            .map_err(|_| CheckpointError::InvalidGitData("blob size is not UTF-8".to_owned()))?;
        size_text
            .trim()
            .parse::<usize>()
            .map_err(|_| CheckpointError::InvalidGitData("blob size is not numeric".to_owned()))
    }

    async fn payload_size(
        &self,
        before_tree: &str,
        after_tree: &str,
        changed_paths: &[String],
    ) -> Result<usize, CheckpointError> {
        let mut total = 0_usize;
        for path in changed_paths {
            for tree in [before_tree, after_tree] {
                if let Some(entry) = self.tree_entry(tree, path).await? {
                    ensure_regular_blob(Some(&entry), path)?;
                    total = total.saturating_add(self.blob_size(&entry.object_id).await?);
                }
            }
        }
        Ok(total)
    }

    async fn retain_segments(
        &self,
        id: u64,
        segments: &[CheckpointSegment],
    ) -> Result<(), CheckpointError> {
        for (index, segment) in segments.iter().enumerate() {
            let before_side = format!("segments/{index}/before");
            self.retain_tree(id, &before_side, &segment.before_tree)
                .await?;
            let after_side = format!("segments/{index}/after");
            self.retain_tree(id, &after_side, &segment.after_tree)
                .await?;
        }
        Ok(())
    }

    async fn release_segments(&self, id: u64, segments: &[CheckpointSegment]) {
        for index in 0..segments.len() {
            let before_side = format!("segments/{index}/before");
            let _ = self.release_tree(id, &before_side).await;
            let after_side = format!("segments/{index}/after");
            let _ = self.release_tree(id, &after_side).await;
        }
    }

    async fn retain_tree(
        &self,
        id: u64,
        side: &str,
        object_id: &str,
    ) -> Result<(), CheckpointError> {
        let reference = checkpoint_ref(id, side);
        self.run_git(&["update-ref", &reference, object_id], None)
            .await?;
        Ok(())
    }

    async fn release_tree(&self, id: u64, side: &str) -> Result<(), CheckpointError> {
        let reference = checkpoint_ref(id, side);
        self.run_git(&["update-ref", "-d", &reference], None)
            .await?;
        Ok(())
    }

    async fn run_git(
        &self,
        args: &[&str],
        temporary_index: Option<&Path>,
    ) -> Result<Vec<u8>, CheckpointError> {
        self.run_git_with_limit(args, temporary_index, MAX_GIT_OUTPUT_BYTES)
            .await
    }

    async fn run_git_with_limit(
        &self,
        args: &[&str],
        temporary_index: Option<&Path>,
        limit_bytes: usize,
    ) -> Result<Vec<u8>, CheckpointError> {
        let operation = args.join(" ");
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_DIR", git_path(&self.snapshot_git_dir))
            .env("GIT_WORK_TREE", git_path(&self.root));
        if let Some(index) = temporary_index {
            command.env("GIT_INDEX_FILE", index);
        }
        let output = timeout(self.git_timeout, command.output())
            .await
            .map_err(|_| CheckpointError::GitTimeout {
                operation: operation.clone(),
                seconds: self.git_timeout.as_secs(),
            })?
            .map_err(|source| CheckpointError::Io {
                path: self.root.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(CheckpointError::Git {
                operation,
                message: bounded_lossy(&output.stderr, 8 * 1024),
            });
        }
        if output.stdout.len() > limit_bytes {
            return Err(CheckpointError::GitOutputTooLarge {
                operation,
                limit_bytes,
            });
        }
        Ok(output.stdout)
    }

    async fn ensure_repository(&self) -> Result<(), CheckpointError> {
        if self.snapshot_git_dir.join("HEAD").is_file() {
            return Ok(());
        }
        fs::create_dir_all(&self.data_dir).map_err(|source| CheckpointError::Io {
            path: self.data_dir.clone(),
            source,
        })?;
        let operation = "init private checkpoint object store".to_owned();
        let output = timeout(
            self.git_timeout,
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(git_path(&self.snapshot_git_dir))
                .current_dir(&self.root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| CheckpointError::GitTimeout {
            operation: operation.clone(),
            seconds: self.git_timeout.as_secs(),
        })?
        .map_err(|source| CheckpointError::Io {
            path: self.snapshot_git_dir.clone(),
            source,
        })?;
        if !output.status.success() {
            return Err(CheckpointError::Git {
                operation,
                message: bounded_lossy(&output.stderr, 8 * 1024),
            });
        }
        Ok(())
    }

    fn persist_records(&self) -> Result<(), CheckpointError> {
        let mut temporary =
            NamedTempFile::new_in(&self.data_dir).map_err(|source| CheckpointError::Io {
                path: self.data_dir.clone(),
                source,
            })?;
        for record in &self.records {
            serde_json::to_writer(&mut temporary, record)?;
            temporary
                .write_all(b"\n")
                .map_err(|source| CheckpointError::Io {
                    path: temporary.path().to_path_buf(),
                    source,
                })?;
        }
        temporary.flush().map_err(|source| CheckpointError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|source| CheckpointError::Io {
                path: temporary.path().to_path_buf(),
                source,
            })?;
        temporary
            .persist(&self.journal_path)
            .map_err(|error| CheckpointError::Io {
                path: self.journal_path.clone(),
                source: error.error,
            })?;
        Ok(())
    }
}

fn resolve_git_dir(root: &Path) -> Result<Option<PathBuf>, CheckpointError> {
    let marker = root.join(".git");
    if marker.is_dir() {
        return fs::canonicalize(&marker)
            .map(Some)
            .map_err(|source| CheckpointError::Io {
                path: marker,
                source,
            });
    }
    if !marker.is_file() {
        return Ok(None);
    }
    let value = fs::read_to_string(&marker).map_err(|source| CheckpointError::Io {
        path: marker.clone(),
        source,
    })?;
    let path = value
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            CheckpointError::InvalidGitData("worktree .git file has no gitdir".to_owned())
        })?;
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let path = fs::canonicalize(&path).map_err(|source| CheckpointError::Io {
        path: path.clone(),
        source,
    })?;
    if !path.is_dir() {
        return Err(CheckpointError::InvalidGitData(
            "worktree gitdir is not a directory".to_owned(),
        ));
    }
    Ok(Some(path))
}

fn load_records(path: &Path) -> Result<VecDeque<CheckpointRecord>, CheckpointError> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(VecDeque::new());
        }
        Err(source) => {
            return Err(CheckpointError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let mut records = VecDeque::new();
    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| CheckpointError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CheckpointRecord>(&line) {
            Ok(record) if valid_checkpoint_record(&record) => {
                records.push_back(record);
                while records.len() > MAX_CHECKPOINTS {
                    records.pop_front();
                }
            }
            Ok(_) => tracing::warn!(
                line = line_number.saturating_add(1),
                "ignored unsupported checkpoint record"
            ),
            Err(error) => tracing::warn!(
                line = line_number.saturating_add(1),
                %error,
                "ignored incomplete checkpoint journal record"
            ),
        }
    }
    Ok(records)
}

fn valid_checkpoint_record(record: &CheckpointRecord) -> bool {
    if record.version != CHECKPOINT_FORMAT_VERSION
        || record.id == 0
        || record.prompt.len() > MAX_PROMPT_BYTES
        || record.changed_paths.len() > MAX_CHANGED_PATHS
        || record.segments.len() > MAX_CHANGED_PATHS
    {
        return false;
    }
    let mut changed_paths = BTreeSet::new();
    for segment in &record.segments {
        if !valid_object_id(&segment.before_tree)
            || !valid_object_id(&segment.after_tree)
            || segment.changed_paths.len() > MAX_CHANGED_PATHS
        {
            return false;
        }
        for path in &segment.changed_paths {
            if path.is_empty() || path.contains('\0') {
                return false;
            }
            changed_paths.insert(path.as_str());
            if changed_paths.len() > MAX_CHANGED_PATHS {
                return false;
            }
        }
    }
    record
        .changed_paths
        .iter()
        .map(String::as_str)
        .eq(changed_paths)
}

fn parse_changed_paths(output: &[u8]) -> Result<Vec<String>, CheckpointError> {
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    let mut paths = Vec::new();
    while let Some(status) = fields.next() {
        if status.len() != 1 || !matches!(status[0], b'A' | b'D' | b'M' | b'T' | b'U') {
            return Err(CheckpointError::InvalidGitData(format!(
                "unexpected diff status {}",
                bounded_lossy(status, 32)
            )));
        }
        let path = fields
            .next()
            .ok_or_else(|| CheckpointError::InvalidGitData("diff status has no path".to_owned()))?;
        let path = String::from_utf8(path.to_vec()).map_err(|_| {
            CheckpointError::InvalidGitData("non-UTF-8 paths cannot be rewound safely".to_owned())
        })?;
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn parse_tree_entry(
    output: &[u8],
    expected_path: &str,
) -> Result<Option<GitTreeEntry>, CheckpointError> {
    if output.is_empty() {
        return Ok(None);
    }
    let entry = output.strip_suffix(&[0]).unwrap_or(output);
    let separator = entry
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| {
            CheckpointError::InvalidGitData("ls-tree entry has no path separator".to_owned())
        })?;
    let (header, path_with_separator) = entry.split_at(separator);
    let path = path_with_separator
        .get(1..)
        .ok_or_else(|| CheckpointError::InvalidGitData("ls-tree path is missing".to_owned()))?;
    if path != expected_path.as_bytes() {
        return Err(CheckpointError::InvalidGitData(
            "ls-tree returned a different path".to_owned(),
        ));
    }
    let header = std::str::from_utf8(header)
        .map_err(|_| CheckpointError::InvalidGitData("ls-tree header is not UTF-8".to_owned()))?;
    let mut fields = header.split_whitespace();
    let mode = fields
        .next()
        .ok_or_else(|| CheckpointError::InvalidGitData("ls-tree mode is missing".to_owned()))?;
    let object_type = fields
        .next()
        .ok_or_else(|| CheckpointError::InvalidGitData("ls-tree type is missing".to_owned()))?;
    let object_id = fields.next().ok_or_else(|| {
        CheckpointError::InvalidGitData("ls-tree object ID is missing".to_owned())
    })?;
    if object_type != "blob" || !valid_object_id(object_id) || fields.next().is_some() {
        return Err(CheckpointError::InvalidGitData(
            "ls-tree entry is not a regular blob".to_owned(),
        ));
    }
    Ok(Some(GitTreeEntry {
        mode: mode.to_owned(),
        object_id: object_id.to_owned(),
    }))
}

fn ensure_regular_blob(entry: Option<&GitTreeEntry>, path: &str) -> Result<(), CheckpointError> {
    if let Some(entry) = entry
        && !matches!(entry.mode.as_str(), "100644" | "100755")
    {
        return Err(CheckpointError::InvalidGitData(format!(
            "{path} has unsupported Git mode {}",
            entry.mode
        )));
    }
    Ok(())
}

fn parse_object_id(output: &[u8]) -> Result<String, CheckpointError> {
    let value = std::str::from_utf8(output)
        .map_err(|_| CheckpointError::InvalidGitData("object ID is not UTF-8".to_owned()))?
        .trim();
    if !valid_object_id(value) {
        return Err(CheckpointError::InvalidGitData(
            "object ID is not hexadecimal".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn valid_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checkpoint_ref(id: u64, side: &str) -> String {
    format!("refs/decode/checkpoints/{id}/{side}")
}

fn preview_prompt(prompt: &str) -> String {
    let mut preview = prompt.chars().take(120).collect::<String>();
    if prompt.chars().count() > 120 {
        preview.push('…');
    }
    preview
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    value[..end].to_owned()
}

fn bounded_lossy(bytes: &[u8], max_bytes: usize) -> String {
    let end = bytes.len().min(max_bytes);
    let mut value = String::from_utf8_lossy(&bytes[..end]).into_owned();
    if bytes.len() > max_bytes {
        value.push_str(" …[truncated]");
    }
    value
}

fn git_path(path: &Path) -> &Path {
    dunce::simplified(path)
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command, time::Duration};

    use tempfile::TempDir;

    use super::{
        CHECKPOINT_FORMAT_VERSION, CheckpointRecord, CheckpointStore, MAX_CHANGED_PATHS,
        parse_changed_paths,
    };
    use crate::agent::state::AgentState;

    fn git_workspace() -> Result<TempDir, Box<dyn std::error::Error>> {
        let workspace = TempDir::new()?;
        let status = Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(workspace.path())
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        Ok(workspace)
    }

    fn git(repository: &std::path::Path, args: &[&str]) -> Result<(), Box<dyn std::error::Error>> {
        let status = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .status()?;
        if !status.success() {
            return Err(format!("git {} failed", args.join(" ")).into());
        }
        Ok(())
    }

    #[test]
    fn name_status_parser_is_nul_safe() -> Result<(), Box<dyn std::error::Error>> {
        let paths = parse_changed_paths(b"M\0src/main.rs\0A\0new file.txt\0")?;
        assert_eq!(paths, ["new file.txt", "src/main.rs"]);
        Ok(())
    }

    #[test]
    fn linked_worktrees_support_checkpoints() -> Result<(), Box<dyn std::error::Error>> {
        let repository = git_workspace()?;
        fs::write(repository.path().join("tracked.txt"), "base")?;
        git(repository.path(), &["add", "tracked.txt"])?;
        git(
            repository.path(),
            &[
                "-c",
                "user.name=Checkpoint Test",
                "-c",
                "user.email=checkpoint@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ],
        )?;
        let worktree_parent = TempDir::new()?;
        let worktree = worktree_parent.path().join("linked");
        git(
            repository.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                worktree.to_string_lossy().as_ref(),
            ],
        )?;
        assert!(worktree.join(".git").is_file());

        assert!(CheckpointStore::open(&worktree, Duration::from_secs(10))?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn oversized_checkpoint_records_are_ignored_on_reload()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        store.begin_tool_segment().await?;
        let record = CheckpointRecord {
            version: CHECKPOINT_FORMAT_VERSION,
            id: 1,
            created_at: chrono::Utc::now(),
            prompt: "oversized".to_owned(),
            changed_paths: (0..=MAX_CHANGED_PATHS)
                .map(|index| format!("file-{index}"))
                .collect(),
            segments: Vec::new(),
            state_before: AgentState::new(),
            session_id: None,
        };
        fs::write(&store.journal_path, serde_json::to_vec(&record)?)?;
        drop(store);

        let reloaded = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        assert!(reloaded.summaries().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rewind_restores_exact_agent_changes_and_created_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("existing.txt"), "before")?;
        let mut store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        let state = AgentState::new();
        let mut pending = store.begin("change files", &state, None).await?;
        let before = store.begin_tool_segment().await?;
        fs::write(workspace.path().join("existing.txt"), "after")?;
        fs::write(workspace.path().join("created.txt"), "created")?;
        store.finish_tool_segment(&mut pending, before).await?;
        let summary = store.commit(pending).await?;

        let result = store.rewind(summary.id).await?;
        assert_eq!(
            fs::read_to_string(workspace.path().join("existing.txt"))?,
            "before"
        );
        assert!(!workspace.path().join("created.txt").exists());
        assert_eq!(result.report.restored_files.len(), 2);
        assert!(result.report.preserved_conflicts.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rewind_preserves_manual_changes_after_agent_turn()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let path = workspace.path().join("file.txt");
        fs::write(&path, "before")?;
        let mut store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        let mut pending = store.begin("agent edit", &AgentState::new(), None).await?;
        let before = store.begin_tool_segment().await?;
        fs::write(&path, "agent")?;
        store.finish_tool_segment(&mut pending, before).await?;
        let summary = store.commit(pending).await?;
        fs::write(&path, "manual")?;

        let result = store.rewind(summary.id).await?;
        assert_eq!(fs::read_to_string(path)?, "manual");
        assert!(result.report.restored_files.is_empty());
        assert_eq!(result.report.preserved_conflicts.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn manual_edit_between_agent_actions_survives_reverse_segments()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let path = workspace.path().join("file.txt");
        fs::write(&path, "original")?;
        let mut store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        let mut pending = store.begin("two actions", &AgentState::new(), None).await?;

        let first_before = store.begin_tool_segment().await?;
        fs::write(&path, "agent-one")?;
        store
            .finish_tool_segment(&mut pending, first_before)
            .await?;
        fs::write(&path, "manual-between")?;
        let second_before = store.begin_tool_segment().await?;
        fs::write(&path, "agent-two")?;
        store
            .finish_tool_segment(&mut pending, second_before)
            .await?;
        let summary = store.commit(pending).await?;

        let result = store.rewind(summary.id).await?;
        assert_eq!(fs::read_to_string(path)?, "manual-between");
        assert_eq!(result.report.preserved_conflicts.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn rewind_only_discards_checkpoints_from_the_active_session()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("a.txt"), "before-a")?;
        fs::write(workspace.path().join("b.txt"), "before-b")?;
        let mut store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;

        store.set_active_session(Some("session-a".to_owned()));
        let mut first = store
            .begin("change a", &AgentState::new(), Some("session-a".to_owned()))
            .await?;
        let before = store.begin_tool_segment().await?;
        fs::write(workspace.path().join("a.txt"), "after-a")?;
        store.finish_tool_segment(&mut first, before).await?;
        let first = store.commit(first).await?;

        store.set_active_session(Some("session-b".to_owned()));
        let mut second = store
            .begin("change b", &AgentState::new(), Some("session-b".to_owned()))
            .await?;
        let before = store.begin_tool_segment().await?;
        fs::write(workspace.path().join("b.txt"), "after-b")?;
        store.finish_tool_segment(&mut second, before).await?;
        let second = store.commit(second).await?;

        store.set_active_session(Some("session-a".to_owned()));
        let result = store.rewind(first.id).await?;
        assert_eq!(result.report.discarded_checkpoints, 1);
        store.set_active_session(Some("session-b".to_owned()));
        assert_eq!(store.summaries()[0].id, second.id);
        Ok(())
    }

    #[tokio::test]
    async fn failed_rewind_journal_update_keeps_in_memory_metadata()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let path = workspace.path().join("file.txt");
        fs::write(&path, "before")?;
        let mut store = CheckpointStore::open(workspace.path(), Duration::from_secs(10))?
            .ok_or("checkpoint store was not available")?;
        let mut pending = store.begin("change", &AgentState::new(), None).await?;
        let before = store.begin_tool_segment().await?;
        fs::write(&path, "after")?;
        store.finish_tool_segment(&mut pending, before).await?;
        let summary = store.commit(pending).await?;

        let invalid_target = store.data_dir.join("directory-target");
        fs::create_dir(&invalid_target)?;
        store.journal_path = invalid_target;
        let result = store.rewind(summary.id).await?;

        assert_eq!(result.report.preserved_conflicts.len(), 1);
        assert_eq!(store.summaries()[0].id, summary.id);
        Ok(())
    }
}
