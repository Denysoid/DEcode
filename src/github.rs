use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;
use tokio::{process::Command, time::timeout};

const MAX_GH_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub enabled: bool,
    pub program: String,
    pub timeout: Duration,
    pub max_pull_requests: usize,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            program: "gh".to_owned(),
            timeout: Duration::from_secs(30),
            max_pull_requests: 50,
        }
    }
}

impl GitHubConfig {
    pub fn validate(&self) -> Result<(), GitHubError> {
        validate_config(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSummary {
    pub number: u64,
    pub title: String,
    pub state: String,
    pub url: String,
    pub head: String,
    pub base: String,
    pub author: String,
    pub draft: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitHubSnapshot {
    pub enabled: bool,
    pub repository: Option<String>,
    pub repository_url: Option<String>,
    pub pull_requests: Arc<[PullRequestSummary]>,
    pub busy: bool,
    pub status: String,
    pub revision: u64,
}

#[derive(Debug, Error)]
pub enum GitHubError {
    #[error("GitHub integration is disabled")]
    Disabled,
    #[error("invalid GitHub CLI configuration: {0}")]
    InvalidConfig(String),
    #[error("failed to start GitHub CLI `{program}`: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("GitHub CLI timed out after {0}s")]
    Timeout(u64),
    #[error("GitHub CLI output exceeded the {MAX_GH_OUTPUT_BYTES}-byte safety limit")]
    OutputTooLarge,
    #[error("GitHub CLI failed: {0}")]
    Command(String),
    #[error("invalid GitHub CLI JSON: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug)]
pub struct GitHubManager {
    config: GitHubConfig,
    workspace: PathBuf,
    snapshot: GitHubSnapshot,
}

impl GitHubManager {
    pub fn new(config: GitHubConfig, workspace: &Path) -> Result<Self, GitHubError> {
        config.validate()?;
        Ok(Self {
            snapshot: GitHubSnapshot {
                enabled: config.enabled,
                status: if config.enabled {
                    "GitHub integration ready; refresh to inspect pull requests".to_owned()
                } else {
                    "GitHub integration disabled in trusted configuration".to_owned()
                },
                ..GitHubSnapshot::default()
            },
            config,
            workspace: workspace.to_path_buf(),
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> GitHubSnapshot {
        self.snapshot.clone()
    }

    pub async fn refresh(&mut self) -> Result<GitHubSnapshot, GitHubError> {
        self.ensure_enabled()?;
        self.snapshot.busy = true;
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        let result = self.refresh_inner().await;
        self.snapshot.busy = false;
        match result {
            Ok((repository, repository_url, pull_requests)) => {
                self.snapshot.repository = Some(repository);
                self.snapshot.repository_url = Some(repository_url);
                self.snapshot.pull_requests = Arc::from(pull_requests);
                self.snapshot.status = format!(
                    "Loaded {} open pull request(s)",
                    self.snapshot.pull_requests.len()
                );
                self.snapshot.revision = self.snapshot.revision.saturating_add(1);
                Ok(self.snapshot())
            }
            Err(error) => {
                self.snapshot.status = format!("GitHub refresh failed: {error}");
                self.snapshot.revision = self.snapshot.revision.saturating_add(1);
                Err(error)
            }
        }
    }

    pub async fn create_draft_from_commits(&mut self) -> Result<GitHubSnapshot, GitHubError> {
        self.ensure_enabled()?;
        self.run(&["pr", "create", "--draft", "--fill"]).await?;
        self.refresh().await
    }

    pub async fn checkout(&mut self, number: u64) -> Result<GitHubSnapshot, GitHubError> {
        self.ensure_enabled()?;
        if number == 0 {
            return Err(GitHubError::InvalidConfig(
                "pull request number must be positive".to_owned(),
            ));
        }
        self.run(&["pr", "checkout", &number.to_string()]).await?;
        self.refresh().await
    }

    pub async fn open(&self, number: u64) -> Result<(), GitHubError> {
        self.ensure_enabled()?;
        if !self
            .snapshot
            .pull_requests
            .iter()
            .any(|pull_request| pull_request.number == number)
        {
            return Err(GitHubError::InvalidConfig(
                "pull request is not present in the current bounded snapshot".to_owned(),
            ));
        }
        self.run(&["pr", "view", &number.to_string(), "--web"])
            .await
            .map(|_| ())
    }

    async fn refresh_inner(
        &self,
    ) -> Result<(String, String, Vec<PullRequestSummary>), GitHubError> {
        let repository: RepositoryWire = serde_json::from_slice(
            &self
                .run(&["repo", "view", "--json", "nameWithOwner,url"])
                .await?,
        )?;
        let max = self.config.max_pull_requests.to_string();
        let pull_requests: Vec<PullRequestWire> = serde_json::from_slice(
            &self
                .run(&[
                    "pr",
                    "list",
                    "--state",
                    "open",
                    "--limit",
                    &max,
                    "--json",
                    "number,title,state,url,headRefName,baseRefName,isDraft,author",
                ])
                .await?,
        )?;
        let pull_requests = pull_requests
            .into_iter()
            .map(PullRequestSummary::from)
            .collect();
        Ok((repository.name_with_owner, repository.url, pull_requests))
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, GitHubError> {
        let mut command = Command::new(&self.config.program);
        command
            .args(args)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let output = timeout(self.config.timeout, command.output())
            .await
            .map_err(|_| GitHubError::Timeout(self.config.timeout.as_secs()))?
            .map_err(|source| GitHubError::Spawn {
                program: self.config.program.clone(),
                source,
            })?;
        if output.stdout.len().saturating_add(output.stderr.len()) > MAX_GH_OUTPUT_BYTES {
            return Err(GitHubError::OutputTooLarge);
        }
        if !output.status.success() {
            return Err(GitHubError::Command(bounded_stderr(&output.stderr)));
        }
        Ok(output.stdout)
    }

    fn ensure_enabled(&self) -> Result<(), GitHubError> {
        self.config
            .enabled
            .then_some(())
            .ok_or(GitHubError::Disabled)
    }
}

fn validate_config(config: &GitHubConfig) -> Result<(), GitHubError> {
    if config.program.trim().is_empty()
        || config.program.len() > 4_096
        || config.program.chars().any(char::is_control)
    {
        return Err(GitHubError::InvalidConfig(
            "program must contain visible text and be at most 4096 bytes".to_owned(),
        ));
    }
    if config.timeout.is_zero() || config.timeout > Duration::from_secs(300) {
        return Err(GitHubError::InvalidConfig(
            "timeout must be between 1 and 300 seconds".to_owned(),
        ));
    }
    if !(1..=100).contains(&config.max_pull_requests) {
        return Err(GitHubError::InvalidConfig(
            "max_pull_requests must be between 1 and 100".to_owned(),
        ));
    }
    Ok(())
}

fn bounded_stderr(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(2_000).collect()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryWire {
    name_with_owner: String,
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestWire {
    number: u64,
    title: String,
    state: String,
    url: String,
    head_ref_name: String,
    base_ref_name: String,
    is_draft: bool,
    author: Option<AuthorWire>,
}

#[derive(Debug, Deserialize)]
struct AuthorWire {
    login: String,
}

impl From<PullRequestWire> for PullRequestSummary {
    fn from(value: PullRequestWire) -> Self {
        Self {
            number: value.number,
            title: value.title,
            state: value.state,
            url: value.url,
            head: value.head_ref_name,
            base: value.base_ref_name,
            author: value
                .author
                .map_or_else(|| "unknown".to_owned(), |a| a.login),
            draft: value.is_draft,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{GitHubConfig, GitHubManager};

    #[test]
    fn configuration_is_bounded_and_disabled_snapshot_is_explicit()
    -> Result<(), Box<dyn std::error::Error>> {
        let manager = GitHubManager::new(GitHubConfig::default(), &std::env::current_dir()?)?;
        assert!(!manager.snapshot().enabled);
        assert!(manager.snapshot().status.contains("disabled"));

        let invalid = GitHubConfig {
            enabled: true,
            timeout: Duration::from_secs(301),
            ..GitHubConfig::default()
        };
        assert!(GitHubManager::new(invalid, &std::env::current_dir()?).is_err());
        Ok(())
    }
}
