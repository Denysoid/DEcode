use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{io::AsyncReadExt as _, process::Command, time::timeout};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthStr as _;

use crate::privacy::PrivacyShield;

use super::state::TurnId;

pub const MAX_REVIEW_REPORTS: usize = 20;
pub const MAX_REVIEW_FINDINGS: usize = 64;
pub const MAX_REVIEW_DIFF_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REVIEW_CHUNK_BYTES: usize = 64 * 1024;
const MIN_REVIEW_CHUNK_BYTES: usize = 1_024;
const MAX_GIT_DIAGNOSTIC_BYTES: usize = 64 * 1024;
const MAX_CHANGED_PATH_BYTES: usize = 1024 * 1024;
const MAX_SUMMARY_BYTES: usize = 32 * 1024;
const MAX_TITLE_BYTES: usize = 512;
const MAX_BODY_BYTES: usize = 16 * 1024;
const MAX_FIX_BYTES: usize = 16 * 1024;

#[derive(Debug, Error)]
pub enum ReviewError {
    #[error("review snapshot Git operation `{operation}` timed out after {seconds}s")]
    GitTimeout { operation: String, seconds: u64 },
    #[error("review snapshot Git operation `{operation}` was cancelled")]
    Cancelled { operation: String },
    #[error("review snapshot Git operation `{operation}` failed: {message}")]
    Git { operation: String, message: String },
    #[error("review snapshot output from `{operation}` exceeded {limit_bytes} bytes")]
    GitOutputTooLarge {
        operation: String,
        limit_bytes: usize,
    },
    #[error("review snapshot I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("review snapshot output from `{0}` is not valid UTF-8")]
    InvalidUtf8(String),
    #[error(
        "review snapshot contains {count} path(s) blocked by Privacy Shield; first blocked path: {first}"
    )]
    SensitivePaths { count: usize, first: String },
    #[error("review diff offset {offset} is not a valid UTF-8 boundary within {length} bytes")]
    InvalidOffset { offset: usize, length: usize },
    #[error("review report must use the active diff digest {expected}, not {actual}")]
    StaleSnapshot { expected: String, actual: String },
    #[error("review summary must contain visible text and be at most {MAX_SUMMARY_BYTES} bytes")]
    InvalidSummary,
    #[error("review accepts at most {MAX_REVIEW_FINDINGS} findings")]
    TooManyFindings,
    #[error(
        "review verdict and findings disagree: pass requires zero findings and changes_requested requires at least one"
    )]
    VerdictMismatch,
    #[error("review finding {index} has an invalid {field}")]
    InvalidFinding { index: usize, field: &'static str },
    #[error("review contains the same finding more than once")]
    DuplicateFinding,
    #[error("turn {0} already submitted a structured review")]
    DuplicateTurn(TurnId),
    #[error("review report {0} does not exist")]
    ReportNotFound(u64),
    #[error("review report {id} changed (expected revision {expected}, current {actual})")]
    StaleRevision { id: u64, expected: u64, actual: u64 },
    #[error("review finding {finding_id} does not exist in report {report_id}")]
    FindingNotFound { report_id: u64, finding_id: u64 },
    #[error("review finding {0} has already been decided")]
    AlreadyDecided(u64),
    #[error("review identifier space is exhausted")]
    IdentifierExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewVerdict {
    Pass,
    ChangesRequested,
}

impl std::fmt::Display for ReviewVerdict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Pass => "pass",
            Self::ChangesRequested => "changes requested",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl std::fmt::Display for ReviewSeverity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingDisposition {
    Open,
    Accepted,
    Dismissed,
    FixQueued,
}

impl std::fmt::Display for ReviewFindingDisposition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Open => "open",
            Self::Accepted => "accepted",
            Self::Dismissed => "dismissed",
            Self::FixQueued => "fix queued",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFindingDecision {
    Accept,
    Dismiss,
    QueueFix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingInput {
    pub severity: ReviewSeverity,
    pub title: String,
    pub body: String,
    pub path: String,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub suggested_fix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitReviewArguments {
    pub snapshot_sha256: String,
    pub verdict: ReviewVerdict,
    pub summary: String,
    pub findings: Vec<ReviewFindingInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: u64,
    pub severity: ReviewSeverity,
    pub title: String,
    pub body: String,
    pub path: String,
    pub line_start: Option<u32>,
    pub line_end: Option<u32>,
    pub suggested_fix: String,
    pub disposition: ReviewFindingDisposition,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewReport {
    pub id: u64,
    pub revision: u64,
    pub turn_id: TurnId,
    pub snapshot_sha256: String,
    pub changed_paths: Vec<String>,
    pub diff_bytes: usize,
    pub verdict: ReviewVerdict,
    pub summary: String,
    pub findings: Vec<ReviewFinding>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewState {
    revision: u64,
    next_report_id: u64,
    next_finding_id: u64,
    reports: Vec<ReviewReport>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCatalogSnapshot {
    pub revision: u64,
    pub reports: std::sync::Arc<[ReviewReport]>,
}

impl ReviewCatalogSnapshot {
    #[must_use]
    pub fn latest(&self) -> Option<&ReviewReport> {
        self.reports.last()
    }

    #[must_use]
    pub fn open_findings(&self) -> usize {
        self.reports
            .iter()
            .flat_map(|report| report.findings.iter())
            .filter(|finding| finding.disposition == ReviewFindingDisposition::Open)
            .count()
    }
}

impl ReviewState {
    pub(crate) fn submit(
        &mut self,
        turn_id: TurnId,
        snapshot: &DiffSnapshot,
        arguments: SubmitReviewArguments,
    ) -> Result<ReviewReport, ReviewError> {
        if arguments.snapshot_sha256 != snapshot.sha256 {
            return Err(ReviewError::StaleSnapshot {
                expected: snapshot.sha256.clone(),
                actual: arguments.snapshot_sha256,
            });
        }
        if self.reports.iter().any(|report| report.turn_id == turn_id) {
            return Err(ReviewError::DuplicateTurn(turn_id));
        }
        validate_visible(&arguments.summary, MAX_SUMMARY_BYTES)
            .map_err(|()| ReviewError::InvalidSummary)?;
        if arguments.findings.len() > MAX_REVIEW_FINDINGS {
            return Err(ReviewError::TooManyFindings);
        }
        if matches!(arguments.verdict, ReviewVerdict::Pass) != arguments.findings.is_empty() {
            return Err(ReviewError::VerdictMismatch);
        }
        let mut identities = BTreeSet::new();
        let mut validated_findings = Vec::with_capacity(arguments.findings.len());
        for (index, input) in arguments.findings.into_iter().enumerate() {
            let input = validate_finding(index, input)?;
            if !snapshot
                .changed_paths
                .iter()
                .any(|path| path == &input.path)
            {
                return Err(ReviewError::InvalidFinding {
                    index,
                    field: "path not present in captured diff",
                });
            }
            let identity = (
                input.path.clone(),
                input.line_start,
                input.title.to_ascii_lowercase(),
            );
            if !identities.insert(identity) {
                return Err(ReviewError::DuplicateFinding);
            }
            validated_findings.push(input);
        }
        let finding_count = u64::try_from(validated_findings.len())
            .map_err(|_| ReviewError::IdentifierExhausted)?;
        let next_finding_id = if finding_count == 0 {
            self.next_finding_id
        } else {
            let after_existing = self
                .reports
                .iter()
                .flat_map(|report| report.findings.iter())
                .map(|finding| finding.id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or(ReviewError::IdentifierExhausted)?;
            let first_finding_id = self.next_finding_id.max(after_existing).max(1);
            first_finding_id
                .checked_add(finding_count)
                .ok_or(ReviewError::IdentifierExhausted)?
        };
        let after_existing_report = self
            .reports
            .iter()
            .map(|report| report.id)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(ReviewError::IdentifierExhausted)?;
        let report_id = self.next_report_id.max(after_existing_report).max(1);
        let next_report_id = report_id
            .checked_add(1)
            .ok_or(ReviewError::IdentifierExhausted)?;
        let first_finding_id = if finding_count == 0 {
            self.next_finding_id
        } else {
            next_finding_id.saturating_sub(finding_count)
        };
        let mut findings = Vec::with_capacity(validated_findings.len());
        for (offset, input) in validated_findings.into_iter().enumerate() {
            let offset = u64::try_from(offset).map_err(|_| ReviewError::IdentifierExhausted)?;
            let id = first_finding_id
                .checked_add(offset)
                .ok_or(ReviewError::IdentifierExhausted)?;
            findings.push(ReviewFinding {
                id,
                severity: input.severity,
                title: input.title,
                body: input.body,
                path: input.path,
                line_start: input.line_start,
                line_end: input.line_end,
                suggested_fix: input.suggested_fix,
                disposition: ReviewFindingDisposition::Open,
                decided_at: None,
            });
        }
        let report = ReviewReport {
            id: report_id,
            revision: 1,
            turn_id,
            snapshot_sha256: snapshot.sha256.clone(),
            changed_paths: snapshot.changed_paths.clone(),
            diff_bytes: snapshot.diff.len(),
            verdict: arguments.verdict,
            summary: arguments.summary,
            findings,
            created_at: Utc::now(),
        };
        self.next_finding_id = next_finding_id;
        self.next_report_id = next_report_id;
        self.reports.push(report.clone());
        if self.reports.len() > MAX_REVIEW_REPORTS {
            let overflow = self.reports.len().saturating_sub(MAX_REVIEW_REPORTS);
            self.reports.drain(..overflow);
        }
        self.bump_revision();
        Ok(report)
    }

    pub fn clear(&mut self) {
        self.reports.clear();
        self.next_report_id = 0;
        self.next_finding_id = 0;
        self.bump_revision();
    }

    #[must_use]
    pub fn submitted_for_turn(&self, turn_id: TurnId) -> bool {
        self.reports.iter().any(|report| report.turn_id == turn_id)
    }

    pub fn decide(
        &mut self,
        report_id: u64,
        expected_revision: u64,
        finding_id: u64,
        decision: ReviewFindingDecision,
    ) -> Result<(), ReviewError> {
        let report = self
            .reports
            .iter_mut()
            .find(|report| report.id == report_id)
            .ok_or(ReviewError::ReportNotFound(report_id))?;
        if report.revision != expected_revision {
            return Err(ReviewError::StaleRevision {
                id: report_id,
                expected: expected_revision,
                actual: report.revision,
            });
        }
        let finding = report
            .findings
            .iter_mut()
            .find(|finding| finding.id == finding_id)
            .ok_or(ReviewError::FindingNotFound {
                report_id,
                finding_id,
            })?;
        if finding.disposition != ReviewFindingDisposition::Open {
            return Err(ReviewError::AlreadyDecided(finding_id));
        }
        finding.disposition = match decision {
            ReviewFindingDecision::Accept => ReviewFindingDisposition::Accepted,
            ReviewFindingDecision::Dismiss => ReviewFindingDisposition::Dismissed,
            ReviewFindingDecision::QueueFix => ReviewFindingDisposition::FixQueued,
        };
        finding.decided_at = Some(Utc::now());
        report.revision = report.revision.saturating_add(1);
        self.bump_revision();
        Ok(())
    }

    pub fn fix_prompt(
        &self,
        report_id: u64,
        expected_revision: u64,
        finding_id: u64,
    ) -> Result<String, ReviewError> {
        let report = self
            .reports
            .iter()
            .find(|report| report.id == report_id)
            .ok_or(ReviewError::ReportNotFound(report_id))?;
        if report.revision != expected_revision {
            return Err(ReviewError::StaleRevision {
                id: report_id,
                expected: expected_revision,
                actual: report.revision,
            });
        }
        let finding = report
            .findings
            .iter()
            .find(|finding| finding.id == finding_id)
            .ok_or(ReviewError::FindingNotFound {
                report_id,
                finding_id,
            })?;
        if finding.disposition != ReviewFindingDisposition::Open {
            return Err(ReviewError::AlreadyDecided(finding_id));
        }
        let lines = match (finding.line_start, finding.line_end) {
            (Some(start), Some(end)) if start != end => format!("lines {start}-{end}"),
            (Some(line), _) => format!("line {line}"),
            _ => "the reviewed diff".to_owned(),
        };
        Ok(format!(
            "Fix accepted code-review finding from immutable diff {}.\nSeverity: {}\nLocation: {} ({})\nTitle: {}\nEvidence: {}\nSuggested direction: {}\nRe-read the current file before editing because the workspace may have changed since the review; do not apply stale line numbers blindly.",
            report.snapshot_sha256,
            finding.severity,
            finding.path,
            lines,
            finding.title,
            finding.body,
            finding.suggested_fix,
        ))
    }

    #[must_use]
    pub fn snapshot(&self) -> ReviewCatalogSnapshot {
        ReviewCatalogSnapshot {
            revision: self.revision,
            reports: std::sync::Arc::from(self.reports.clone()),
        }
    }

    fn bump_revision(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiffSnapshot {
    pub sha256: String,
    pub changed_paths: Vec<String>,
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DiffChunk<'a> {
    pub snapshot_sha256: &'a str,
    pub changed_paths: &'a [String],
    pub total_bytes: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub complete: bool,
    pub diff: &'a str,
}

impl DiffSnapshot {
    #[cfg(test)]
    pub(crate) async fn capture(
        root: &Path,
        git_timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Self, ReviewError> {
        Self::capture_with_privacy(root, git_timeout, cancel, None).await
    }

    pub(crate) async fn capture_with_privacy(
        root: &Path,
        git_timeout: Duration,
        cancel: &CancellationToken,
        privacy: Option<&PrivacyShield>,
    ) -> Result<Self, ReviewError> {
        let root = std::fs::canonicalize(root).map_err(|source| ReviewError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        let temporary = tempfile::tempdir().map_err(|source| ReviewError::Io {
            path: std::env::temp_dir(),
            source,
        })?;
        let index = temporary.path().join("review.index");
        let has_head = run_git(
            &root,
            &index,
            &["rev-parse", "--verify", "HEAD"],
            MAX_GIT_DIAGNOSTIC_BYTES,
            git_timeout,
            cancel,
            true,
        )
        .await?
        .success;
        let read_tree = if has_head {
            vec!["read-tree", "HEAD"]
        } else {
            vec!["read-tree", "--empty"]
        };
        require_git_success(
            run_git(
                &root,
                &index,
                &read_tree,
                MAX_GIT_DIAGNOSTIC_BYTES,
                git_timeout,
                cancel,
                false,
            )
            .await?,
        )?;
        require_git_success(
            run_git(
                &root,
                &index,
                &["add", "-A", "--", "."],
                MAX_GIT_DIAGNOSTIC_BYTES,
                git_timeout,
                cancel,
                false,
            )
            .await?,
        )?;
        let mut diff_args = vec![
            "diff",
            "--cached",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--full-index",
        ];
        if has_head {
            diff_args.push("HEAD");
        }
        diff_args.extend(["--", "."]);
        let diff_result = run_git(
            &root,
            &index,
            &diff_args,
            MAX_REVIEW_DIFF_BYTES,
            git_timeout,
            cancel,
            false,
        )
        .await?;
        let diff = require_git_success(diff_result)?;
        let diff =
            String::from_utf8(diff).map_err(|_| ReviewError::InvalidUtf8("git diff".to_owned()))?;

        let mut names_args = vec!["diff", "--cached", "--name-only", "-z", "--no-renames"];
        if has_head {
            names_args.push("HEAD");
        }
        names_args.extend(["--", "."]);
        let names = require_git_success(
            run_git(
                &root,
                &index,
                &names_args,
                MAX_CHANGED_PATH_BYTES,
                git_timeout,
                cancel,
                false,
            )
            .await?,
        )?;
        let changed_paths = parse_nul_paths(names)?;
        let blocked = changed_paths
            .iter()
            .filter(|path| {
                privacy
                    .is_some_and(|shield| !shield.allows_relative(Path::new(path.as_str()), false))
            })
            .collect::<Vec<_>>();
        if let Some(first) = blocked.first() {
            return Err(ReviewError::SensitivePaths {
                count: blocked.len(),
                first: (*first).clone(),
            });
        }
        let sha256 = format!("{:x}", Sha256::digest(diff.as_bytes()));
        drop(temporary);
        Ok(Self {
            sha256,
            changed_paths,
            diff,
        })
    }

    pub(crate) fn chunk(
        &self,
        offset: usize,
        requested_bytes: usize,
    ) -> Result<DiffChunk<'_>, ReviewError> {
        if offset > self.diff.len() || !self.diff.is_char_boundary(offset) {
            return Err(ReviewError::InvalidOffset {
                offset,
                length: self.diff.len(),
            });
        }
        let requested_bytes = requested_bytes.clamp(MIN_REVIEW_CHUNK_BYTES, MAX_REVIEW_CHUNK_BYTES);
        let mut end = offset.saturating_add(requested_bytes).min(self.diff.len());
        while end > offset && !self.diff.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        let complete = end == self.diff.len();
        Ok(DiffChunk {
            snapshot_sha256: &self.sha256,
            changed_paths: &self.changed_paths,
            total_bytes: self.diff.len(),
            offset,
            next_offset: (!complete).then_some(end),
            complete,
            diff: &self.diff[offset..end],
        })
    }
}

#[derive(Debug)]
struct GitResult {
    operation: String,
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn require_git_success(result: GitResult) -> Result<Vec<u8>, ReviewError> {
    if result.success {
        return Ok(result.stdout);
    }
    Err(ReviewError::Git {
        operation: result.operation,
        message: bounded_lossy(&result.stderr),
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_git(
    root: &Path,
    index: &Path,
    args: &[&str],
    max_stdout_bytes: usize,
    git_timeout: Duration,
    cancel: &CancellationToken,
    allow_failure: bool,
) -> Result<GitResult, ReviewError> {
    let operation = format!("git {}", args.join(" "));
    let mut command = Command::new("git");
    command
        .args(args.iter().map(OsStr::new))
        .current_dir(root)
        .env("GIT_INDEX_FILE", index)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| ReviewError::Io {
        path: PathBuf::from("git"),
        source,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| ReviewError::Io {
        path: PathBuf::from("git stdout"),
        source: std::io::Error::other("Git stdout pipe was unavailable"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| ReviewError::Io {
        path: PathBuf::from("git stderr"),
        source: std::io::Error::other("Git stderr pipe was unavailable"),
    })?;
    let stdout_task = tokio::spawn(read_bounded(stdout, max_stdout_bytes));
    let stderr_task = tokio::spawn(read_bounded(stderr, MAX_GIT_DIAGNOSTIC_BYTES));
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            return Err(ReviewError::Cancelled { operation });
        }
        result = timeout(git_timeout, child.wait()) => match result {
            Ok(Ok(status)) => status,
            Ok(Err(source)) => {
                stdout_task.abort();
                stderr_task.abort();
                return Err(ReviewError::Io { path: PathBuf::from("git"), source });
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                return Err(ReviewError::GitTimeout {
                    operation,
                    seconds: git_timeout.as_secs(),
                });
            }
        }
    };
    let stdout = join_reader(stdout_task, &operation, max_stdout_bytes).await?;
    let stderr = join_reader(stderr_task, &operation, MAX_GIT_DIAGNOSTIC_BYTES).await?;
    let result = GitResult {
        operation,
        success: status.success(),
        stdout,
        stderr,
    };
    if !result.success && !allow_failure {
        require_git_success(result).map(|stdout| GitResult {
            operation: String::new(),
            success: true,
            stdout,
            stderr: Vec::new(),
        })
    } else {
        Ok(result)
    }
}

async fn read_bounded<R>(mut reader: R, limit: usize) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "bounded Git output limit exceeded",
            ));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

async fn join_reader(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    operation: &str,
    limit: usize,
) -> Result<Vec<u8>, ReviewError> {
    match task.await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(source)) if source.kind() == std::io::ErrorKind::FileTooLarge => {
            Err(ReviewError::GitOutputTooLarge {
                operation: operation.to_owned(),
                limit_bytes: limit,
            })
        }
        Ok(Err(source)) => Err(ReviewError::Io {
            path: PathBuf::from(operation),
            source,
        }),
        Err(source) => Err(ReviewError::Io {
            path: PathBuf::from(operation),
            source: std::io::Error::other(source.to_string()),
        }),
    }
}

fn parse_nul_paths(bytes: Vec<u8>) -> Result<Vec<String>, ReviewError> {
    let mut paths = Vec::new();
    for raw in bytes.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| ReviewError::InvalidUtf8("git diff --name-only".to_owned()))?;
        paths.push(path.replace('\\', "/"));
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn validate_finding(
    index: usize,
    mut input: ReviewFindingInput,
) -> Result<ReviewFindingInput, ReviewError> {
    validate_visible(&input.title, MAX_TITLE_BYTES).map_err(|()| ReviewError::InvalidFinding {
        index,
        field: "title",
    })?;
    validate_visible(&input.body, MAX_BODY_BYTES).map_err(|()| ReviewError::InvalidFinding {
        index,
        field: "body",
    })?;
    if input.suggested_fix.len() > MAX_FIX_BYTES || input.suggested_fix.contains('\0') {
        return Err(ReviewError::InvalidFinding {
            index,
            field: "suggested_fix",
        });
    }
    input.path = normalize_relative_path(&input.path).ok_or(ReviewError::InvalidFinding {
        index,
        field: "path",
    })?;
    if input.line_start == Some(0)
        || input.line_end == Some(0)
        || matches!((input.line_start, input.line_end), (None, Some(_)))
        || matches!((input.line_start, input.line_end), (Some(start), Some(end)) if end < start)
    {
        return Err(ReviewError::InvalidFinding {
            index,
            field: "line range",
        });
    }
    Ok(input)
}

fn normalize_relative_path(path: &str) -> Option<String> {
    if path.trim().is_empty() || path.len() > 4_096 || path.contains('\0') {
        return None;
    }
    let normalized = path.replace('\\', "/");
    let candidate = Path::new(&normalized);
    if candidate.is_absolute()
        || candidate.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let parts = candidate
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            Component::CurDir => None,
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => None,
        })
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn validate_visible(value: &str, max_bytes: usize) -> Result<(), ()> {
    if value.trim().is_empty()
        || value.width() == 0
        || value.len() > max_bytes
        || value.contains('\0')
        || value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return Err(());
    }
    Ok(())
}

fn bounded_lossy(bytes: &[u8]) -> String {
    let message = String::from_utf8_lossy(bytes);
    let trimmed = message.trim();
    if trimmed.is_empty() {
        "Git returned a non-success status without diagnostics".to_owned()
    } else {
        trimmed.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, process::Command as StdCommand};

    use tempfile::{TempDir, tempdir};

    use super::*;

    fn snapshot() -> DiffSnapshot {
        DiffSnapshot {
            sha256: "abc123".to_owned(),
            changed_paths: vec!["src/lib.rs".to_owned()],
            diff: "diff --git a/src/lib.rs b/src/lib.rs\n+fn fixed() {}\n".to_owned(),
        }
    }

    fn arguments() -> SubmitReviewArguments {
        SubmitReviewArguments {
            snapshot_sha256: "abc123".to_owned(),
            verdict: ReviewVerdict::ChangesRequested,
            summary: "One actionable correctness issue.".to_owned(),
            findings: vec![ReviewFindingInput {
                severity: ReviewSeverity::High,
                title: "Incorrect boundary".to_owned(),
                body: "The new branch skips the final element.".to_owned(),
                path: "src\\lib.rs".to_owned(),
                line_start: Some(14),
                line_end: Some(16),
                suggested_fix: "Use an inclusive range and add a regression test.".to_owned(),
            }],
        }
    }

    #[test]
    fn reports_are_snapshot_bound_revision_bound_and_queueable() -> Result<(), ReviewError> {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let report = state.submit(7, &snapshot, arguments())?;
        assert_eq!(report.findings[0].path, "src/lib.rs");
        assert!(matches!(
            state.decide(
                report.id,
                report.revision.saturating_add(1),
                report.findings[0].id,
                ReviewFindingDecision::Accept
            ),
            Err(ReviewError::StaleRevision { .. })
        ));
        let prompt = state.fix_prompt(report.id, report.revision, report.findings[0].id)?;
        assert!(prompt.contains("Re-read the current file"));
        state.decide(
            report.id,
            report.revision,
            report.findings[0].id,
            ReviewFindingDecision::QueueFix,
        )?;
        assert_eq!(state.snapshot().open_findings(), 0);
        Ok(())
    }

    #[test]
    fn stale_digest_and_unsafe_paths_are_rejected() {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let mut stale = arguments();
        stale.snapshot_sha256 = "old".to_owned();
        assert!(matches!(
            state.submit(1, &snapshot, stale),
            Err(ReviewError::StaleSnapshot { .. })
        ));
        let mut unsafe_path = arguments();
        unsafe_path.findings[0].path = "../../outside.rs".to_owned();
        assert!(matches!(
            state.submit(2, &snapshot, unsafe_path),
            Err(ReviewError::InvalidFinding { field: "path", .. })
        ));
    }

    #[test]
    fn line_end_without_a_start_is_rejected() {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let mut invalid = arguments();
        invalid.findings[0].line_start = None;
        invalid.findings[0].line_end = Some(16);

        assert!(matches!(
            state.submit(1, &snapshot, invalid),
            Err(ReviewError::InvalidFinding {
                field: "line range",
                ..
            })
        ));
    }

    #[test]
    fn rejected_report_does_not_consume_finding_ids() {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let mut invalid = arguments();
        let mut second = invalid.findings[0].clone();
        second.path = "../outside.rs".to_owned();
        invalid.findings.push(second);

        assert!(state.submit(1, &snapshot, invalid).is_err());
        assert_eq!(state.next_finding_id, 0);
    }

    #[test]
    fn review_summary_must_have_a_visible_glyph() {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let mut invalid = arguments();
        invalid.summary = "\u{200b}\u{2060}".to_owned();

        assert!(matches!(
            state.submit(1, &snapshot, invalid),
            Err(ReviewError::InvalidSummary)
        ));
    }

    #[test]
    fn exhausted_identifiers_fail_closed() {
        let snapshot = snapshot();
        let mut findings_exhausted = ReviewState {
            next_finding_id: u64::MAX,
            ..ReviewState::default()
        };
        assert!(
            findings_exhausted
                .submit(1, &snapshot, arguments())
                .is_err()
        );

        let mut reports_exhausted = ReviewState {
            next_report_id: u64::MAX,
            ..ReviewState::default()
        };
        assert!(reports_exhausted.submit(1, &snapshot, arguments()).is_err());
    }

    #[test]
    fn stale_persisted_counters_cannot_reuse_report_or_finding_ids() -> Result<(), ReviewError> {
        let snapshot = snapshot();
        let mut state = ReviewState::default();
        let first = state.submit(1, &snapshot, arguments())?;
        state.next_report_id = 0;
        state.next_finding_id = 0;

        let second = state.submit(2, &snapshot, arguments())?;

        assert!(second.id > first.id);
        assert!(second.findings[0].id > first.findings[0].id);
        Ok(())
    }

    #[test]
    fn chunks_preserve_utf8_boundaries_and_report_next_offset() -> Result<(), ReviewError> {
        let mut snapshot = snapshot();
        snapshot.diff = format!("{}ž{}", "a".repeat(1_023), "b".repeat(2_000));
        let first = snapshot.chunk(0, 1_024)?;
        let next = first.next_offset.ok_or(ReviewError::InvalidOffset {
            offset: 0,
            length: 0,
        })?;
        assert!(snapshot.diff.is_char_boundary(next));
        assert!(!first.complete);
        assert!(snapshot.chunk(next, 1_024)?.offset > 0);
        Ok(())
    }

    #[tokio::test]
    async fn capture_includes_tracked_and_untracked_changes()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let status = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        fs::write(root.path().join("tracked.txt"), "before\n")?;
        let status = StdCommand::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(root.path())
            .status()?;
        if !status.success() {
            return Err("git add failed".into());
        }
        let status = StdCommand::new("git")
            .args([
                "-c",
                "user.name=review-test",
                "-c",
                "user.email=review@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "base",
            ])
            .current_dir(root.path())
            .status()?;
        if !status.success() {
            return Err("git commit failed".into());
        }
        fs::write(root.path().join("tracked.txt"), "after\n")?;
        fs::write(root.path().join("new.txt"), "untracked\n")?;
        let snapshot = DiffSnapshot::capture(
            root.path(),
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;
        assert_eq!(snapshot.changed_paths, ["new.txt", "tracked.txt"]);
        assert!(snapshot.diff.contains("after"));
        assert!(snapshot.diff.contains("untracked"));
        fs::write(root.path().join(".env"), "TOKEN=never-send\n")?;
        let privacy = crate::privacy::PrivacyShield::load_project_only(root.path())?;
        let guarded = DiffSnapshot::capture_with_privacy(
            root.path(),
            Duration::from_secs(10),
            &CancellationToken::new(),
            Some(&privacy),
        )
        .await;
        assert!(matches!(guarded, Err(ReviewError::SensitivePaths { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn capture_respects_gitignore_for_large_generated_build_trees()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let status = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        fs::write(root.path().join(".gitignore"), "/.cargo-target-*/\n")?;
        fs::write(root.path().join("tracked.txt"), "review me\n")?;
        let generated = root.path().join(".cargo-target-language");
        fs::create_dir(&generated)?;
        fs::write(
            generated.join("large.bin"),
            vec![b'x'; MAX_REVIEW_DIFF_BYTES + 1],
        )?;

        let snapshot = DiffSnapshot::capture(
            root.path(),
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;

        assert!(snapshot.changed_paths.contains(&".gitignore".to_owned()));
        assert!(snapshot.changed_paths.contains(&"tracked.txt".to_owned()));
        assert!(!snapshot.diff.contains("large.bin"));
        Ok(())
    }

    #[tokio::test]
    async fn capture_compacts_large_untracked_binary_files()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempdir()?;
        let status = StdCommand::new("git")
            .args(["init", "--quiet"])
            .current_dir(root.path())
            .status()?;
        if !status.success() {
            return Err("git init failed".into());
        }
        let mut state = 0x1234_5678_u32;
        let binary = (0..=MAX_REVIEW_DIFF_BYTES)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect::<Vec<_>>();
        fs::write(root.path().join("large.bin"), binary)?;

        let snapshot = DiffSnapshot::capture(
            root.path(),
            Duration::from_secs(10),
            &CancellationToken::new(),
        )
        .await?;

        assert_eq!(snapshot.changed_paths, ["large.bin"]);
        assert!(
            snapshot
                .diff
                .contains("Binary files /dev/null and b/large.bin differ")
        );
        assert!(snapshot.diff.len() < MAX_REVIEW_CHUNK_BYTES);
        Ok(())
    }

    #[test]
    fn temp_dir_type_is_send_safe_for_async_capture() {
        fn assert_send<T: Send>() {}
        assert_send::<TempDir>();
    }
}
