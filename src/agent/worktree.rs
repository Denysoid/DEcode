use std::{
    ffi::{OsStr, OsString},
    fmt::Write as _,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    task::JoinError,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::tools::{
    PatchReview, PatchReviewError, SandboxError, SandboxRoot, sandbox::CheckpointRestore,
};

const MAX_GIT_OUTPUT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CHANGED_FILES: usize = 256;
const MAX_CHANGED_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_TOTAL_CHANGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum WorktreeError {
    #[error("managed worktree root must be outside workspace {workspace:?}: {worktree_root:?}")]
    RootInsideWorkspace {
        workspace: PathBuf,
        worktree_root: PathBuf,
    },
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("git command timed out after {seconds}s: {operation}")]
    GitTimeout { operation: String, seconds: u64 },
    #[error("git command failed ({operation}): {message}")]
    GitFailed { operation: String, message: String },
    #[error("git command output exceeded {limit_bytes} bytes: {operation}")]
    GitOutputTooLarge {
        operation: String,
        limit_bytes: usize,
    },
    #[error("git worker task failed: {0}")]
    GitWorker(#[from] JoinError),
    #[error("git returned non-UTF-8 output for {operation}")]
    GitOutputEncoding { operation: String },
    #[error("managed worktree path escaped its root: {0:?}")]
    PathEscape(PathBuf),
    #[error("managed worktree already exists: {0:?}")]
    AlreadyExists(PathBuf),
    #[error("managed worktree is missing: {0:?}")]
    Missing(PathBuf),
    #[error("managed worktree base commit is invalid: {0:?}")]
    InvalidBaseCommit(String),
    #[error("worktree change list is malformed")]
    MalformedChangeList,
    #[error("worktree changed more than {limit} files")]
    TooManyChanges { limit: usize },
    #[error("changed file {path:?} exceeds {limit_bytes} bytes")]
    ChangedFileTooLarge { path: String, limit_bytes: usize },
    #[error("worktree changes exceed {limit_bytes} bytes in total")]
    ChangesTooLarge { limit_bytes: usize },
    #[error("changed path is not valid UTF-8")]
    NonUtf8Path,
    #[error("changed path {0:?} is a symbolic link or escaped the worktree")]
    UnsafeChangedPath(String),
    #[error("binary change {0:?} requires whole-file approval")]
    BinaryReview(String),
    #[error("patch review failed: {0}")]
    PatchReview(#[from] PatchReviewError),
    #[error("sandbox rejected worktree integration: {0}")]
    Sandbox(#[from] SandboxError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug, Clone)]
pub struct WorktreeChange {
    pub path: String,
    pub kind: WorktreeChangeKind,
    pub base: Option<Arc<[u8]>>,
    pub result: Option<Arc<[u8]>>,
    pub base_executable: Option<bool>,
    pub result_executable: Option<bool>,
}

impl WorktreeChange {
    #[must_use]
    pub fn is_binary(&self) -> bool {
        self.base
            .as_deref()
            .is_some_and(|bytes| std::str::from_utf8(bytes).is_err())
            || self
                .result
                .as_deref()
                .is_some_and(|bytes| std::str::from_utf8(bytes).is_err())
    }

    pub fn review(&self) -> Result<PatchReview, WorktreeError> {
        if self.is_binary() {
            return Err(WorktreeError::BinaryReview(self.path.clone()));
        }
        let base = self
            .base
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| WorktreeError::BinaryReview(self.path.clone()))?
            .unwrap_or_default();
        let result = self
            .result
            .as_deref()
            .map(std::str::from_utf8)
            .transpose()
            .map_err(|_| WorktreeError::BinaryReview(self.path.clone()))?
            .unwrap_or_default();
        Ok(PatchReview::new(&self.path, base, result))
    }
}

#[derive(Debug, Clone)]
pub struct WorktreeChangeSet {
    pub changes: Arc<[WorktreeChange]>,
    pub digest: [u8; 32],
}

impl WorktreeChangeSet {
    #[must_use]
    pub fn digest_hex(&self) -> String {
        hex_digest(&self.digest)
    }
}

#[derive(Debug, Clone)]
pub struct ManagedWorktree {
    pub path: PathBuf,
    pub base_commit: String,
}

#[derive(Clone)]
pub struct WorktreeManager {
    workspace: PathBuf,
    control_root: PathBuf,
    trees_root: PathBuf,
    git_dir: PathBuf,
    hooks_dir: PathBuf,
    sandbox: SandboxRoot,
    git_timeout: Duration,
}

impl std::fmt::Debug for WorktreeManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorktreeManager")
            .field("workspace", &self.workspace)
            .field("control_root", &self.control_root)
            .field("trees_root", &self.trees_root)
            .field("git_timeout", &self.git_timeout)
            .finish_non_exhaustive()
    }
}

impl WorktreeManager {
    pub async fn open(
        workspace: &Path,
        configured_root: &Path,
        git_timeout: Duration,
    ) -> Result<Self, WorktreeError> {
        let workspace_root = canonical_directory(workspace).await?;
        if configured_root.is_absolute()
            && git_path(configured_root).starts_with(git_path(&workspace_root))
        {
            return Err(WorktreeError::RootInsideWorkspace {
                workspace: workspace_root,
                worktree_root: configured_root.to_path_buf(),
            });
        }
        tokio::fs::create_dir_all(configured_root)
            .await
            .map_err(|source| WorktreeError::Io {
                path: configured_root.to_path_buf(),
                source,
            })?;
        let configured_root = canonical_directory(configured_root).await?;
        if configured_root.starts_with(&workspace_root) {
            return Err(WorktreeError::RootInsideWorkspace {
                workspace: workspace_root,
                worktree_root: configured_root,
            });
        }

        let workspace_key = workspace_key(&workspace_root);
        let control_root = create_direct_child_directory(&configured_root, &workspace_key).await?;
        let trees_root = create_direct_child_directory(&control_root, "trees").await?;
        let git_dir = create_direct_child_directory(&control_root, "snapshot.git").await?;
        let hooks_dir = create_direct_child_directory(&control_root, "disabled-hooks").await?;

        if !git_dir.join("HEAD").is_file() {
            init_bare_repository(&git_dir, &hooks_dir, git_timeout).await?;
        }

        let manager = Self {
            workspace: workspace_root.clone(),
            control_root,
            trees_root,
            git_dir,
            hooks_dir,
            sandbox: SandboxRoot::open(&workspace_root)?,
            git_timeout,
        };
        let _ = manager.run_git(None, ["worktree", "prune"], &[]).await?;
        Ok(manager)
    }

    #[must_use]
    pub fn control_root(&self) -> &Path {
        &self.control_root
    }

    #[must_use]
    pub fn workspace_root(&self) -> &Path {
        &self.workspace
    }

    pub async fn create(&self, id: u64) -> Result<ManagedWorktree, WorktreeError> {
        let path = self.trees_root.join(format!("agent-{id:08}"));
        if path.exists() {
            return Err(WorktreeError::AlreadyExists(path));
        }
        let base_commit = self.capture_workspace_commit(id).await?;
        let arguments = vec![
            OsString::from("worktree"),
            OsString::from("add"),
            OsString::from("--detach"),
            OsString::from("--force"),
            git_path(&path).as_os_str().to_owned(),
            OsString::from(base_commit.clone()),
        ];
        self.run_git_os(None, &arguments, &[]).await?;
        let canonical = canonical_directory(&path).await?;
        let trees_root = canonical_directory(&self.trees_root).await?;
        if !canonical.starts_with(&trees_root) {
            return Err(WorktreeError::PathEscape(canonical));
        }
        Ok(ManagedWorktree {
            path: canonical,
            base_commit,
        })
    }

    pub async fn recover(
        &self,
        id: u64,
        base_commit: String,
    ) -> Result<ManagedWorktree, WorktreeError> {
        if !matches!(base_commit.len(), 40 | 64)
            || !base_commit.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(WorktreeError::InvalidBaseCommit(base_commit));
        }
        let path = self.trees_root.join(format!("agent-{id:08}"));
        self.validate_managed_path(&path).await?;
        // A linked worktree owns a per-worktree HEAD. Passing the private bare
        // repository via `--git-dir` here would resolve its (intentionally
        // unborn) main HEAD instead of the detached HEAD recorded in
        // `<worktree>/.git`. Let Git discover that link from the managed path.
        let arguments = [OsString::from("rev-parse"), OsString::from("HEAD")];
        let output = run_git_bounded(
            None,
            Some(&path),
            &self.hooks_dir,
            &arguments,
            &[],
            MAX_GIT_OUTPUT_BYTES,
            self.git_timeout,
        )
        .await?;
        let actual = std::str::from_utf8(&output.stdout)
            .map_err(|_| WorktreeError::GitOutputEncoding {
                operation: "rev-parse HEAD".to_owned(),
            })?
            .trim();
        if actual != base_commit {
            return Err(WorktreeError::GitFailed {
                operation: "recover managed worktree".to_owned(),
                message: format!(
                    "recorded base commit {base_commit} does not match worktree HEAD {actual}"
                ),
            });
        }
        Ok(ManagedWorktree { path, base_commit })
    }

    pub async fn collect_changes(
        &self,
        worktree: &ManagedWorktree,
    ) -> Result<WorktreeChangeSet, WorktreeError> {
        self.validate_managed_path(&worktree.path).await?;
        let result_tree = self
            .capture_tree(&worktree.path, &worktree.base_commit)
            .await?;
        let output = self
            .run_git(
                None,
                [
                    "diff-tree",
                    "--no-commit-id",
                    "--name-status",
                    "--no-renames",
                    "-r",
                    "-z",
                    &worktree.base_commit,
                    &result_tree,
                ],
                &[],
            )
            .await?;
        let entries = parse_change_list(&output.stdout)?;
        if entries.len() > MAX_CHANGED_FILES {
            return Err(WorktreeError::TooManyChanges {
                limit: MAX_CHANGED_FILES,
            });
        }

        let mut changes = Vec::with_capacity(entries.len());
        let mut total_bytes = 0_usize;
        for (kind, path) in entries {
            let base_blob = if kind == WorktreeChangeKind::Added {
                None
            } else {
                self.read_tree_blob(&worktree.base_commit, &path).await?
            };
            let result_blob = if kind == WorktreeChangeKind::Deleted {
                None
            } else {
                Some(self.read_worktree_file(&worktree.path, &path).await?)
            };
            total_bytes = total_bytes
                .saturating_add(base_blob.as_ref().map_or(0, Vec::len))
                .saturating_add(result_blob.as_ref().map_or(0, Vec::len));
            if total_bytes > MAX_TOTAL_CHANGE_BYTES {
                return Err(WorktreeError::ChangesTooLarge {
                    limit_bytes: MAX_TOTAL_CHANGE_BYTES,
                });
            }
            let base_mode = if kind == WorktreeChangeKind::Added {
                None
            } else {
                self.tree_executable(&worktree.base_commit, &path).await?
            };
            let result_mode = if kind == WorktreeChangeKind::Deleted {
                None
            } else {
                file_executable(&worktree.path.join(Path::new(&path))).await?
            };
            changes.push(WorktreeChange {
                path,
                kind,
                base: base_blob.map(Arc::from),
                result: result_blob.map(Arc::from),
                base_executable: base_mode,
                result_executable: result_mode,
            });
        }
        let digest = change_digest(&changes);
        Ok(WorktreeChangeSet {
            changes: Arc::from(changes),
            digest,
        })
    }

    pub async fn apply_text_decisions(
        &self,
        change: WorktreeChange,
        decisions: Vec<bool>,
        cancel: CancellationToken,
    ) -> Result<(), WorktreeError> {
        let review = change.review()?;
        let selection = review.apply_decisions(&decisions)?;
        let approve_all = selection.approved_hunks == selection.total_hunks;
        let reject_all = selection.total_hunks > 0 && selection.approved_hunks == 0;
        let delete = change.kind == WorktreeChangeKind::Deleted && approve_all;
        let desired = if reject_all {
            change.base.as_deref().map(<[u8]>::to_vec)
        } else {
            (!delete).then_some(selection.replacement.into_bytes())
        };
        let desired_executable = if approve_all {
            change.result_executable
        } else {
            change.base_executable
        };
        let sandbox = self.sandbox.clone();
        tokio::task::spawn_blocking(move || {
            sandbox.checkpoint_compare_and_restore(
                Path::new(&change.path),
                CheckpointRestore {
                    expected_content: change.base.as_deref(),
                    expected_executable: change.base_executable,
                    desired_content: desired.as_deref(),
                    desired_executable,
                    limit_bytes: MAX_CHANGED_FILE_BYTES,
                },
                &cancel,
            )
        })
        .await??;
        Ok(())
    }

    pub async fn apply_binary_whole(
        &self,
        change: WorktreeChange,
        cancel: CancellationToken,
    ) -> Result<(), WorktreeError> {
        let sandbox = self.sandbox.clone();
        tokio::task::spawn_blocking(move || {
            sandbox.checkpoint_compare_and_restore(
                Path::new(&change.path),
                CheckpointRestore {
                    expected_content: change.base.as_deref(),
                    expected_executable: change.base_executable,
                    desired_content: change.result.as_deref(),
                    desired_executable: change.result_executable,
                    limit_bytes: MAX_CHANGED_FILE_BYTES,
                },
                &cancel,
            )
        })
        .await??;
        Ok(())
    }

    pub async fn discard(&self, worktree: &ManagedWorktree) -> Result<(), WorktreeError> {
        self.validate_managed_path(&worktree.path).await?;
        let arguments = vec![
            OsString::from("worktree"),
            OsString::from("remove"),
            OsString::from("--force"),
            git_path(&worktree.path).as_os_str().to_owned(),
        ];
        self.run_git_os(None, &arguments, &[]).await?;
        Ok(())
    }

    async fn capture_workspace_commit(&self, id: u64) -> Result<String, WorktreeError> {
        let tree = self.capture_tree(&self.workspace, "").await?;
        let message = format!("decode sub-agent {id} workspace snapshot");
        let output = self
            .run_git(
                Some(&self.workspace),
                ["commit-tree", &tree, "-m", &message],
                &[
                    ("GIT_AUTHOR_NAME", "DEcode by denysoid Snapshot"),
                    ("GIT_AUTHOR_EMAIL", "snapshot@decode.invalid"),
                    ("GIT_COMMITTER_NAME", "DEcode by denysoid Snapshot"),
                    ("GIT_COMMITTER_EMAIL", "snapshot@decode.invalid"),
                ],
            )
            .await?;
        output.utf8_trimmed("commit-tree")
    }

    async fn capture_tree(&self, worktree: &Path, base: &str) -> Result<String, WorktreeError> {
        let temporary = tempfile::Builder::new()
            .prefix("index-")
            .tempdir_in(&self.control_root)
            .map_err(|source| WorktreeError::Io {
                path: self.control_root.clone(),
                source,
            })?;
        let index = temporary.path().join("index");
        let index_value = git_path(&index).to_string_lossy().into_owned();
        if base.is_empty() {
            self.run_git(
                Some(worktree),
                ["read-tree", "--empty"],
                &[("GIT_INDEX_FILE", &index_value)],
            )
            .await?;
        } else {
            self.run_git(
                Some(worktree),
                ["read-tree", base],
                &[("GIT_INDEX_FILE", &index_value)],
            )
            .await?;
        }
        self.run_git(
            Some(worktree),
            ["add", "-A", "--", "."],
            &[("GIT_INDEX_FILE", &index_value)],
        )
        .await?;
        let output = self
            .run_git(
                Some(worktree),
                ["write-tree"],
                &[("GIT_INDEX_FILE", &index_value)],
            )
            .await?;
        output.utf8_trimmed("write-tree")
    }

    async fn read_tree_blob(
        &self,
        treeish: &str,
        path: &str,
    ) -> Result<Option<Vec<u8>>, WorktreeError> {
        let output = self
            .run_git_os(
                None,
                &[
                    OsString::from("--literal-pathspecs"),
                    OsString::from("ls-tree"),
                    OsString::from("-z"),
                    OsString::from(treeish),
                    OsString::from("--"),
                    OsString::from(path),
                ],
                &[],
            )
            .await?;
        if output.stdout.is_empty() {
            return Ok(None);
        }
        let object = parse_ls_tree_object(&output.stdout)?;
        let arguments = [
            OsString::from("cat-file"),
            OsString::from("blob"),
            OsString::from(object),
        ];
        let blob = self
            .run_git_os_with_stdout_limit(None, &arguments, &[], MAX_CHANGED_FILE_BYTES)
            .await
            .map_err(|error| match error {
                WorktreeError::GitOutputTooLarge { .. } => WorktreeError::ChangedFileTooLarge {
                    path: path.to_owned(),
                    limit_bytes: MAX_CHANGED_FILE_BYTES,
                },
                other => other,
            })?;
        if blob.stdout.len() > MAX_CHANGED_FILE_BYTES {
            return Err(WorktreeError::ChangedFileTooLarge {
                path: path.to_owned(),
                limit_bytes: MAX_CHANGED_FILE_BYTES,
            });
        }
        Ok(Some(blob.stdout))
    }

    async fn tree_executable(
        &self,
        treeish: &str,
        path: &str,
    ) -> Result<Option<bool>, WorktreeError> {
        let output = self
            .run_git_os(
                None,
                &[
                    OsString::from("--literal-pathspecs"),
                    OsString::from("ls-tree"),
                    OsString::from("-z"),
                    OsString::from(treeish),
                    OsString::from("--"),
                    OsString::from(path),
                ],
                &[],
            )
            .await?;
        if output.stdout.is_empty() {
            return Ok(None);
        }
        let mode = output
            .stdout
            .split(|byte| *byte == b' ')
            .next()
            .ok_or(WorktreeError::MalformedChangeList)?;
        Ok(Some(mode == b"100755"))
    }

    async fn read_worktree_file(
        &self,
        worktree: &Path,
        relative: &str,
    ) -> Result<Vec<u8>, WorktreeError> {
        let path = worktree.join(Path::new(relative));
        let metadata =
            tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|source| WorktreeError::Io {
                    path: path.clone(),
                    source,
                })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(WorktreeError::UnsafeChangedPath(relative.to_owned()));
        }
        let canonical =
            tokio::fs::canonicalize(&path)
                .await
                .map_err(|source| WorktreeError::Io {
                    path: path.clone(),
                    source,
                })?;
        if !canonical.starts_with(worktree) {
            return Err(WorktreeError::UnsafeChangedPath(relative.to_owned()));
        }
        if metadata.len() > MAX_CHANGED_FILE_BYTES as u64 {
            return Err(WorktreeError::ChangedFileTooLarge {
                path: relative.to_owned(),
                limit_bytes: MAX_CHANGED_FILE_BYTES,
            });
        }
        tokio::fs::read(&canonical)
            .await
            .map_err(|source| WorktreeError::Io {
                path: canonical,
                source,
            })
    }

    async fn validate_managed_path(&self, path: &Path) -> Result<(), WorktreeError> {
        let metadata = tokio::fs::symlink_metadata(path).await.map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                WorktreeError::Missing(path.to_path_buf())
            } else {
                WorktreeError::Io {
                    path: path.to_path_buf(),
                    source,
                }
            }
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(WorktreeError::PathEscape(path.to_path_buf()));
        }
        let canonical = canonical_directory(path).await?;
        let root = canonical_directory(&self.trees_root).await?;
        if canonical.parent() != Some(root.as_path()) {
            return Err(WorktreeError::PathEscape(canonical));
        }
        Ok(())
    }

    async fn run_git<'a, I>(
        &self,
        worktree: Option<&Path>,
        args: I,
        environment: &[(&str, &str)],
    ) -> Result<GitOutput, WorktreeError>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let arguments = args.into_iter().map(OsString::from).collect::<Vec<_>>();
        self.run_git_os(worktree, &arguments, environment).await
    }

    async fn run_git_os(
        &self,
        worktree: Option<&Path>,
        args: &[OsString],
        environment: &[(&str, &str)],
    ) -> Result<GitOutput, WorktreeError> {
        self.run_git_os_with_stdout_limit(worktree, args, environment, MAX_GIT_OUTPUT_BYTES)
            .await
    }

    async fn run_git_os_with_stdout_limit(
        &self,
        worktree: Option<&Path>,
        args: &[OsString],
        environment: &[(&str, &str)],
        stdout_limit: usize,
    ) -> Result<GitOutput, WorktreeError> {
        run_git_bounded(
            Some(&self.git_dir),
            worktree,
            &self.hooks_dir,
            args,
            environment,
            stdout_limit,
            self.git_timeout,
        )
        .await
    }
}

#[derive(Debug)]
struct GitOutput {
    stdout: Vec<u8>,
}

impl GitOutput {
    fn utf8_trimmed(self, operation: &str) -> Result<String, WorktreeError> {
        String::from_utf8(self.stdout)
            .map(|value| value.trim().to_owned())
            .map_err(|_| WorktreeError::GitOutputEncoding {
                operation: operation.to_owned(),
            })
    }
}

async fn init_bare_repository(
    git_dir: &Path,
    hooks_dir: &Path,
    git_timeout: Duration,
) -> Result<(), WorktreeError> {
    let _parent = git_dir
        .parent()
        .ok_or_else(|| WorktreeError::PathEscape(git_dir.to_path_buf()))?;
    let arguments = vec![
        OsString::from("init"),
        OsString::from("--bare"),
        git_path(git_dir).as_os_str().to_owned(),
    ];
    run_git_bounded(
        None,
        None,
        hooks_dir,
        &arguments,
        &[],
        MAX_GIT_OUTPUT_BYTES,
        git_timeout,
    )
    .await?;
    Ok(())
}

async fn run_git_bounded(
    git_dir: Option<&Path>,
    worktree: Option<&Path>,
    hooks_dir: &Path,
    args: &[OsString],
    environment: &[(&str, &str)],
    stdout_limit: usize,
    duration: Duration,
) -> Result<GitOutput, WorktreeError> {
    let operation = args
        .iter()
        .map(|value| value.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let mut command = Command::new("git");
    if let Some(git_dir) = git_dir {
        command.arg("--git-dir").arg(git_path(git_dir));
    }
    if let Some(worktree) = worktree {
        command
            .arg("--work-tree")
            .arg(git_path(worktree))
            .current_dir(git_path(worktree));
    } else if let Some(git_dir) = git_dir {
        command.current_dir(git_path(git_dir));
    }
    command
        .arg("-c")
        .arg(format!("core.hooksPath={}", git_path(hooks_dir).display()))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_NAMESPACE");
    for (name, value) in environment {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|source| WorktreeError::Io {
        path: PathBuf::from("git"),
        source,
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| WorktreeError::GitFailed {
            operation: operation.clone(),
            message: "stdout pipe was unavailable".to_owned(),
        })?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| WorktreeError::GitFailed {
            operation: operation.clone(),
            message: "stderr pipe was unavailable".to_owned(),
        })?;
    let stdout_task = tokio::spawn(read_bounded(stdout, stdout_limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_GIT_OUTPUT_BYTES));
    let status = match timeout(duration, child.wait()).await {
        Ok(result) => result.map_err(|source| WorktreeError::Io {
            path: PathBuf::from("git"),
            source,
        })?,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(WorktreeError::GitTimeout {
                operation,
                seconds: duration.as_secs(),
            });
        }
    };
    let stdout = stdout_task.await??;
    let stderr = stderr_task.await??;
    if !status.success() {
        return Err(WorktreeError::GitFailed {
            operation,
            message: bounded_lossy(&stderr, 8 * 1024),
        });
    }
    Ok(GitOutput { stdout })
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> Result<Vec<u8>, WorktreeError>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|source| WorktreeError::Io {
                path: PathBuf::from("git pipe"),
                source,
            })?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(WorktreeError::GitOutputTooLarge {
                operation: "read git output".to_owned(),
                limit_bytes: limit,
            });
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn canonical_directory(path: &Path) -> Result<PathBuf, WorktreeError> {
    let canonical = tokio::fs::canonicalize(path)
        .await
        .map_err(|source| WorktreeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let metadata = tokio::fs::metadata(&canonical)
        .await
        .map_err(|source| WorktreeError::Io {
            path: canonical.clone(),
            source,
        })?;
    if !metadata.is_dir() {
        return Err(WorktreeError::PathEscape(canonical));
    }
    Ok(canonical)
}

async fn create_direct_child_directory(
    parent: &Path,
    name: &str,
) -> Result<PathBuf, WorktreeError> {
    let requested = parent.join(name);
    tokio::fs::create_dir_all(&requested)
        .await
        .map_err(|source| WorktreeError::Io {
            path: requested.clone(),
            source,
        })?;
    let canonical = canonical_directory(&requested).await?;
    if canonical.parent() != Some(parent) {
        return Err(WorktreeError::PathEscape(canonical));
    }
    Ok(canonical)
}

fn parse_change_list(bytes: &[u8]) -> Result<Vec<(WorktreeChangeKind, String)>, WorktreeError> {
    let fields = bytes
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let (pairs, remainder) = fields.as_chunks::<2>();
    if !remainder.is_empty() {
        return Err(WorktreeError::MalformedChangeList);
    }
    pairs
        .iter()
        .map(|pair| {
            let kind = match pair[0] {
                b"A" => WorktreeChangeKind::Added,
                b"M" | b"T" => WorktreeChangeKind::Modified,
                b"D" => WorktreeChangeKind::Deleted,
                _ => return Err(WorktreeError::MalformedChangeList),
            };
            let path = std::str::from_utf8(pair[1])
                .map_err(|_| WorktreeError::NonUtf8Path)?
                .to_owned();
            Ok((kind, path))
        })
        .collect()
}

fn parse_ls_tree_object(bytes: &[u8]) -> Result<String, WorktreeError> {
    let header = bytes
        .split(|byte| *byte == b'\t')
        .next()
        .ok_or(WorktreeError::MalformedChangeList)?;
    let object = header
        .split(|byte| *byte == b' ')
        .nth(2)
        .ok_or(WorktreeError::MalformedChangeList)?;
    std::str::from_utf8(object)
        .map(str::to_owned)
        .map_err(|_| WorktreeError::MalformedChangeList)
}

fn change_digest(changes: &[WorktreeChange]) -> [u8; 32] {
    let mut digest = Sha256::new();
    for change in changes {
        digest.update(change.path.as_bytes());
        digest.update([0]);
        digest.update([match change.kind {
            WorktreeChangeKind::Added => 1,
            WorktreeChangeKind::Modified => 2,
            WorktreeChangeKind::Deleted => 3,
        }]);
        digest.update([u8::from(change.base_executable.unwrap_or(false))]);
        digest.update([u8::from(change.result_executable.unwrap_or(false))]);
        if let Some(base) = &change.base {
            digest.update(Sha256::digest(base));
        }
        if let Some(result) = &change.result {
            digest.update(Sha256::digest(result));
        }
    }
    digest.finalize().into()
}

fn workspace_key(workspace: &Path) -> String {
    let digest = Sha256::digest(workspace.as_os_str().to_string_lossy().as_bytes());
    format!("workspace-{}", &hex_digest(&digest)[..16])
}

fn hex_digest(digest: &[u8]) -> String {
    let mut output = String::with_capacity(digest.len().saturating_mul(2));
    for byte in digest {
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

async fn file_executable(path: &Path) -> Result<Option<bool>, WorktreeError> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|source| WorktreeError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Ok(Some(metadata.permissions().mode() & 0o111 != 0))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Ok(Some(false))
    }
}

fn bounded_lossy(bytes: &[u8], limit: usize) -> String {
    let prefix = bytes.get(..bytes.len().min(limit)).unwrap_or(bytes);
    String::from_utf8_lossy(prefix).trim().to_owned()
}

fn git_path(path: &Path) -> &Path {
    dunce::simplified(path)
}

fn null_device() -> &'static OsStr {
    #[cfg(windows)]
    {
        OsStr::new("NUL")
    }
    #[cfg(not(windows))]
    {
        OsStr::new("/dev/null")
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command, time::Duration};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{WorktreeChangeKind, WorktreeError, WorktreeManager, workspace_key};

    fn git_workspace() -> Result<TempDir, Box<dyn std::error::Error>> {
        let workspace = tempfile::tempdir()?;
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        Ok(workspace)
    }

    #[cfg(unix)]
    fn link_directory(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(unix)]
    fn unlink_directory(link: &std::path::Path) -> std::io::Result<()> {
        fs::remove_file(link)
    }

    #[cfg(windows)]
    fn link_directory(target: &std::path::Path, link: &std::path::Path) -> std::io::Result<()> {
        let output = Command::new("cmd.exe")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ))
        }
    }

    #[cfg(windows)]
    fn unlink_directory(link: &std::path::Path) -> std::io::Result<()> {
        fs::remove_dir(link)
    }

    #[tokio::test]
    async fn rejects_redirected_workspace_control_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let configured = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let canonical_workspace = fs::canonicalize(workspace.path())?;
        let redirected = configured.path().join(workspace_key(&canonical_workspace));
        link_directory(outside.path(), &redirected)?;

        let result =
            WorktreeManager::open(workspace.path(), configured.path(), Duration::from_secs(10))
                .await;

        assert!(matches!(result, Err(WorktreeError::PathEscape(_))));
        assert!(!outside.path().join("trees").exists());
        Ok(())
    }

    #[tokio::test]
    async fn rejects_a_new_control_root_inside_the_workspace_without_creating_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let configured = workspace.path().join("nested").join("worktrees");

        let result =
            WorktreeManager::open(workspace.path(), &configured, Duration::from_secs(10)).await;

        assert!(matches!(
            result,
            Err(WorktreeError::RootInsideWorkspace { .. })
        ));
        assert!(!configured.exists());
        Ok(())
    }

    #[tokio::test]
    async fn worktree_snapshot_includes_manual_and_untracked_state()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("tracked.txt"), "manual\n")?;
        fs::write(workspace.path().join("untracked.txt"), "present\n")?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;

        let worktree = manager.create(7).await?;
        let recovered = manager.recover(7, worktree.base_commit.clone()).await?;
        assert_eq!(recovered.path, worktree.path);
        assert_eq!(recovered.base_commit, worktree.base_commit);
        let invalid = manager.recover(7, "--not-an-object".to_owned()).await;
        assert!(matches!(invalid, Err(WorktreeError::InvalidBaseCommit(_))));
        assert_eq!(
            fs::read_to_string(worktree.path.join("tracked.txt"))?,
            "manual\n"
        );
        assert_eq!(
            fs::read_to_string(worktree.path.join("untracked.txt"))?,
            "present\n"
        );
        fs::write(worktree.path.join("tracked.txt"), "agent\n")?;
        fs::write(worktree.path.join("created.txt"), "new\n")?;

        let changes = manager.collect_changes(&worktree).await?;
        assert_eq!(changes.changes.len(), 2);
        assert!(changes.changes.iter().any(|change| {
            change.path == "tracked.txt" && change.kind == WorktreeChangeKind::Modified
        }));
        assert!(changes.changes.iter().any(|change| {
            change.path == "created.txt" && change.kind == WorktreeChangeKind::Added
        }));
        manager.discard(&worktree).await?;
        assert!(!worktree.path.exists());
        Ok(())
    }

    #[tokio::test]
    async fn recovery_rejects_a_worktree_path_redirected_to_its_sibling()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("tracked.txt"), "base\n")?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let worktree = manager.create(31).await?;
        let redirected = manager.trees_root.join("agent-00000032");
        link_directory(&worktree.path, &redirected)?;

        let result = manager.recover(32, worktree.base_commit.clone()).await;
        assert!(matches!(result, Err(WorktreeError::PathEscape(_))));

        unlink_directory(&redirected)?;
        manager.discard(&worktree).await?;
        Ok(())
    }

    #[tokio::test]
    async fn integration_is_compare_and_swap_and_preserves_new_manual_edit()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("file.txt"), "base\n")?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let worktree = manager.create(8).await?;
        fs::write(worktree.path.join("file.txt"), "agent\n")?;
        let changes = manager.collect_changes(&worktree).await?;
        let change = changes.changes[0].clone();
        fs::write(workspace.path().join("file.txt"), "manual-after-spawn\n")?;

        let result = manager
            .apply_text_decisions(change, vec![true], CancellationToken::new())
            .await;
        assert!(matches!(result, Err(WorktreeError::Sandbox(_))));
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt"))?,
            "manual-after-spawn\n"
        );
        manager.discard(&worktree).await?;
        Ok(())
    }

    #[tokio::test]
    async fn rejecting_every_hunk_of_an_added_file_preserves_its_absence()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let worktree = manager.create(9).await?;
        fs::write(worktree.path.join("added.txt"), "agent\n")?;

        let changes = manager.collect_changes(&worktree).await?;
        let change = changes
            .changes
            .iter()
            .find(|change| change.path == "added.txt")
            .cloned()
            .ok_or("added file was not collected")?;
        manager
            .apply_text_decisions(change, vec![false], CancellationToken::new())
            .await?;

        assert!(!workspace.path().join("added.txt").exists());
        manager.discard(&worktree).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_still_aborts_a_no_op_text_decision()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let worktree = manager.create(11).await?;
        fs::write(worktree.path.join("added.txt"), "agent\n")?;
        let changes = manager.collect_changes(&worktree).await?;
        let change = changes.changes[0].clone();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = manager
            .apply_text_decisions(change, vec![false], cancel)
            .await;
        assert!(matches!(
            result,
            Err(WorktreeError::Sandbox(
                crate::tools::SandboxError::Cancelled { .. }
            ))
        ));

        manager.discard(&worktree).await?;
        Ok(())
    }

    #[tokio::test]
    async fn collects_modified_files_up_to_the_declared_file_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        let mut contents = vec![b'a'; 3 * 1024 * 1024];
        fs::write(workspace.path().join("large.bin"), &contents)?;
        let managed = tempfile::tempdir()?;
        let manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let worktree = manager.create(10).await?;
        contents[0] = b'b';
        fs::write(worktree.path.join("large.bin"), &contents)?;

        let changes = manager.collect_changes(&worktree).await?;
        let change = changes
            .changes
            .iter()
            .find(|change| change.path == "large.bin")
            .ok_or("large file was not collected")?;
        assert_eq!(
            change.base.as_deref().map(<[u8]>::len),
            Some(contents.len())
        );
        assert_eq!(
            change.result.as_deref().map(<[u8]>::len),
            Some(contents.len())
        );

        manager.discard(&worktree).await?;
        Ok(())
    }

    #[tokio::test]
    async fn nested_writer_integrates_into_parent_before_main_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let workspace = git_workspace()?;
        fs::write(workspace.path().join("file.txt"), "main\n")?;
        let managed = tempfile::tempdir()?;
        let parent_manager =
            WorktreeManager::open(workspace.path(), managed.path(), Duration::from_secs(10))
                .await?;
        let parent = parent_manager.create(21).await?;
        fs::write(parent.path.join("file.txt"), "parent draft\n")?;

        let child_manager =
            WorktreeManager::open(&parent.path, managed.path(), Duration::from_secs(10)).await?;
        let child = child_manager.create(22).await?;
        assert_eq!(
            fs::read_to_string(child.path.join("file.txt"))?,
            "parent draft\n"
        );
        fs::write(child.path.join("file.txt"), "nested child\n")?;

        let child_changes = child_manager.collect_changes(&child).await?;
        assert_eq!(child_changes.changes.len(), 1);
        child_manager
            .apply_text_decisions(
                child_changes.changes[0].clone(),
                vec![true],
                CancellationToken::new(),
            )
            .await?;
        assert_eq!(
            fs::read_to_string(parent.path.join("file.txt"))?,
            "nested child\n"
        );
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt"))?,
            "main\n"
        );

        child_manager.discard(&child).await?;
        drop(child_manager);
        let parent_changes = parent_manager.collect_changes(&parent).await?;
        parent_manager
            .apply_text_decisions(
                parent_changes.changes[0].clone(),
                vec![true],
                CancellationToken::new(),
            )
            .await?;
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt"))?,
            "nested child\n"
        );
        parent_manager.discard(&parent).await?;
        Ok(())
    }
}
