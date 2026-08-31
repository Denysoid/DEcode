use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::StreamExt as _;
use reqwest::{StatusCode, Url, header::CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use unicode_width::UnicodeWidthStr;

const MICROS_PER_USD: u64 = 1_000_000;
const TOKENS_PER_MILLION: u128 = 1_000_000;
const MAX_RATE_USD_PER_MILLION: f64 = 1_000_000.0;
const MAX_DEPLOYMENT_BYTES: usize = 256;
const PRICING_OVERRIDES_VERSION: u32 = 1;
const MAX_PRICING_OVERRIDES_BYTES: u64 = 256 * 1024;
const MAX_PRICING_OVERRIDES: usize = 256;
const MAX_REMOTE_PRICING_BYTES: usize = 16 * 1024 * 1024;
const MAX_REMOTE_PRICING_MODELS: usize = 20_000;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PricingError {
    #[error(
        "deployment name must contain visible text and be at most {MAX_DEPLOYMENT_BYTES} bytes"
    )]
    InvalidDeployment,
    #[error("{field} must be a finite value in 0..={MAX_RATE_USD_PER_MILLION}")]
    InvalidRate { field: &'static str },
    #[error("pricing for deployment {0:?} is configured more than once")]
    DuplicateDeployment(String),
    #[error("long-context threshold must be greater than zero")]
    InvalidLongContextThreshold,
    #[error("pricing catalog JSON is invalid: {0}")]
    InvalidCatalog(String),
    #[error("pricing catalog exceeds its model limit")]
    CatalogTooLarge,
}

#[derive(Debug, Error)]
pub enum PricingRefreshError {
    #[error("pricing catalog URL is invalid or not HTTPS: {0}")]
    InvalidUrl(String),
    #[error("pricing catalog request failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("pricing catalog returned HTTP {0}")]
    Http(u16),
    #[error("pricing catalog content type is not JSON: {0}")]
    ContentType(String),
    #[error("pricing catalog exceeds {MAX_REMOTE_PRICING_BYTES} bytes")]
    TooLarge,
    #[error("pricing catalog request timed out")]
    Timeout,
    #[error("pricing catalog cache has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("pricing catalog cache I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Pricing(#[from] PricingError),
}

#[derive(Debug, Error)]
pub enum PricingStoreError {
    #[error("pricing override path has no parent: {0}")]
    MissingParent(PathBuf),
    #[error("pricing override path must not be a symbolic link: {0}")]
    Symlink(PathBuf),
    #[error("pricing override path is not a regular file: {0}")]
    NotFile(PathBuf),
    #[error("pricing override file is larger than {MAX_PRICING_OVERRIDES_BYTES} bytes: {0}")]
    TooLarge(PathBuf),
    #[error("pricing override file contains more than {MAX_PRICING_OVERRIDES} entries")]
    TooManyEntries,
    #[error("failed to access pricing overrides at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse pricing overrides at {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("unsupported pricing override version {found}; expected {PRICING_OVERRIDES_VERSION}")]
    UnsupportedVersion { found: u32 },
    #[error(transparent)]
    InvalidPricing(#[from] PricingError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PricingTier {
    input_microusd_per_million: u64,
    cached_input_microusd_per_million: u64,
    output_microusd_per_million: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PricingSource {
    PublicCatalog,
    OfficialCatalog,
    Configuration,
    UserOverride,
}

impl PricingSource {
    const fn priority(self) -> u8 {
        match self {
            Self::PublicCatalog => 1,
            Self::OfficialCatalog => 2,
            Self::Configuration => 3,
            Self::UserOverride => 4,
        }
    }

    #[must_use]
    pub const fn is_approximate(self) -> bool {
        matches!(self, Self::PublicCatalog)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::PublicCatalog => "public catalog",
            Self::OfficialCatalog => "official public tariff",
            Self::Configuration => "config.toml",
            Self::UserOverride => "local exact override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingProvenance {
    pub source: PricingSource,
    pub label: String,
    pub updated_at: Option<String>,
}

impl PricingProvenance {
    #[must_use]
    pub const fn is_approximate(&self) -> bool {
        self.source.is_approximate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentPricing {
    deployment: String,
    input_microusd_per_million: u64,
    cached_input_microusd_per_million: u64,
    output_microusd_per_million: u64,
    long_context: Option<(u64, PricingTier)>,
    provenance: PricingProvenance,
}

impl DeploymentPricing {
    pub fn from_usd_per_million(
        deployment: String,
        input: f64,
        cached_input: Option<f64>,
        output: f64,
    ) -> Result<Self, PricingError> {
        if deployment.trim().is_empty()
            || UnicodeWidthStr::width(deployment.trim()) == 0
            || deployment.len() > MAX_DEPLOYMENT_BYTES
            || deployment.chars().any(char::is_control)
        {
            return Err(PricingError::InvalidDeployment);
        }
        let input_microusd_per_million = rate_to_microusd("input_usd_per_million", input)?;
        let cached_input_microusd_per_million = rate_to_microusd(
            "cached_input_usd_per_million",
            cached_input.unwrap_or(input),
        )?;
        let output_microusd_per_million = rate_to_microusd("output_usd_per_million", output)?;
        Ok(Self {
            deployment,
            input_microusd_per_million,
            cached_input_microusd_per_million,
            output_microusd_per_million,
            long_context: None,
            provenance: PricingProvenance {
                source: PricingSource::Configuration,
                label: PricingSource::Configuration.label().to_owned(),
                updated_at: None,
            },
        })
    }

    pub fn with_long_context_tier(
        mut self,
        threshold_tokens: u64,
        input: f64,
        cached_input: Option<f64>,
        output: f64,
    ) -> Result<Self, PricingError> {
        if threshold_tokens == 0 {
            return Err(PricingError::InvalidLongContextThreshold);
        }
        self.long_context = Some((
            threshold_tokens,
            PricingTier {
                input_microusd_per_million: rate_to_microusd(
                    "long_context_input_usd_per_million",
                    input,
                )?,
                cached_input_microusd_per_million: rate_to_microusd(
                    "long_context_cached_input_usd_per_million",
                    cached_input.unwrap_or(input),
                )?,
                output_microusd_per_million: rate_to_microusd(
                    "long_context_output_usd_per_million",
                    output,
                )?,
            },
        ));
        Ok(self)
    }

    #[must_use]
    pub fn deployment(&self) -> &str {
        &self.deployment
    }

    #[must_use]
    pub fn with_provenance(
        mut self,
        source: PricingSource,
        label: impl Into<String>,
        updated_at: Option<String>,
    ) -> Self {
        self.provenance = PricingProvenance {
            source,
            label: label.into(),
            updated_at,
        };
        self
    }

    #[must_use]
    pub fn as_user_override(self) -> Self {
        self.with_provenance(
            PricingSource::UserOverride,
            PricingSource::UserOverride.label(),
            Some(Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)),
        )
    }

    #[must_use]
    pub fn provenance(&self) -> &PricingProvenance {
        &self.provenance
    }

    #[must_use]
    pub const fn long_context_snapshot(&self) -> Option<LongContextPricingSnapshot> {
        match &self.long_context {
            Some((threshold_tokens, tier)) => Some(LongContextPricingSnapshot {
                threshold_tokens: *threshold_tokens,
                rate: PricingRateSnapshot {
                    input_microusd_per_million: tier.input_microusd_per_million,
                    cached_input_microusd_per_million: tier.cached_input_microusd_per_million,
                    output_microusd_per_million: tier.output_microusd_per_million,
                },
            }),
            None => None,
        }
    }

    #[must_use]
    pub const fn rate_snapshot(&self) -> PricingRateSnapshot {
        PricingRateSnapshot {
            input_microusd_per_million: self.input_microusd_per_million,
            cached_input_microusd_per_million: self.cached_input_microusd_per_million,
            output_microusd_per_million: self.output_microusd_per_million,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricingRateSnapshot {
    pub input_microusd_per_million: u64,
    pub cached_input_microusd_per_million: u64,
    pub output_microusd_per_million: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongContextPricingSnapshot {
    pub threshold_tokens: u64,
    pub rate: PricingRateSnapshot,
}

impl PricingRateSnapshot {
    #[must_use]
    pub fn input_usd_per_million(self) -> f64 {
        self.input_microusd_per_million as f64 / MICROS_PER_USD as f64
    }

    #[must_use]
    pub fn cached_input_usd_per_million(self) -> f64 {
        self.cached_input_microusd_per_million as f64 / MICROS_PER_USD as f64
    }

    #[must_use]
    pub fn output_usd_per_million(self) -> f64 {
        self.output_microusd_per_million as f64 / MICROS_PER_USD as f64
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PricingCatalog {
    rates: BTreeMap<String, DeploymentPricing>,
}

impl PricingCatalog {
    pub fn new(entries: Vec<DeploymentPricing>) -> Result<Self, PricingError> {
        let mut rates = BTreeMap::new();
        for entry in entries {
            let deployment = entry.deployment.clone();
            if rates.insert(deployment.clone(), entry).is_some() {
                return Err(PricingError::DuplicateDeployment(deployment));
            }
        }
        Ok(Self { rates })
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        !self.rates.is_empty()
    }

    pub fn upsert(&mut self, pricing: DeploymentPricing) {
        let should_replace = self.rates.get(&pricing.deployment).is_none_or(|existing| {
            pricing.provenance.source.priority() >= existing.provenance.source.priority()
        });
        if should_replace {
            self.rates.insert(pricing.deployment.clone(), pricing);
        }
    }

    pub fn merge(&mut self, other: Self) {
        for pricing in other.rates.into_values() {
            self.upsert(pricing);
        }
    }

    pub fn merge_litellm_json(
        &mut self,
        provider: &str,
        bytes: &[u8],
    ) -> Result<usize, PricingError> {
        self.merge_litellm_json_at(provider, bytes, None)
    }

    fn merge_litellm_json_at(
        &mut self,
        provider: &str,
        bytes: &[u8],
        updated_at: Option<&str>,
    ) -> Result<usize, PricingError> {
        if bytes.len() > MAX_REMOTE_PRICING_BYTES {
            return Err(PricingError::CatalogTooLarge);
        }
        let entries: BTreeMap<String, LiteLlmPricingEntry> = serde_json::from_slice(bytes)
            .map_err(|error| PricingError::InvalidCatalog(error.to_string()))?;
        if entries.len() > MAX_REMOTE_PRICING_MODELS {
            return Err(PricingError::CatalogTooLarge);
        }
        let mut imported = 0_usize;
        let mut staged = Self::default();
        let mut direct_keys = BTreeSet::new();
        let mut aliases = BTreeMap::<String, Option<DeploymentPricing>>::new();
        for (model_key, entry) in entries {
            if !catalog_provider_matches(provider, entry.provider.as_deref()) {
                continue;
            }
            let Some(input) = entry.input_cost_per_token else {
                continue;
            };
            let Some(output) = entry.output_cost_per_token else {
                continue;
            };
            let cached = entry
                .cache_read_input_token_cost
                .map_or(input, |value| value);
            let rate = DeploymentPricing::from_usd_per_million(
                model_key.clone(),
                input * 1_000_000.0,
                Some(cached * 1_000_000.0),
                output * 1_000_000.0,
            )?
            .with_provenance(
                PricingSource::PublicCatalog,
                "LiteLLM public catalog",
                updated_at.map(str::to_owned),
            );
            direct_keys.insert(model_key.clone());
            staged.upsert(rate);
            imported = imported.saturating_add(1);

            if let Some((_, unqualified)) = model_key.split_once('/')
                && !unqualified.is_empty()
            {
                let alias = DeploymentPricing::from_usd_per_million(
                    unqualified.to_owned(),
                    input * 1_000_000.0,
                    Some(cached * 1_000_000.0),
                    output * 1_000_000.0,
                )?
                .with_provenance(
                    PricingSource::PublicCatalog,
                    "LiteLLM public catalog",
                    updated_at.map(str::to_owned),
                );
                match aliases.entry(unqualified.to_owned()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(Some(alias));
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        entry.insert(None);
                    }
                }
            }
        }
        for (alias, pricing) in aliases {
            if !direct_keys.contains(&alias)
                && let Some(pricing) = pricing
            {
                staged.upsert(pricing);
            }
        }
        self.merge(staged);
        Ok(imported)
    }

    #[must_use]
    pub fn snapshot(
        &self,
        ledger: &UsageLedger,
        last_response_tokens: Option<u64>,
    ) -> UsageSnapshot {
        let mut deployments = Vec::with_capacity(ledger.by_deployment.len());
        let mut totals = TokenUsage::default();
        let mut estimated_cost_microusd = 0_u64;
        let mut has_unpriced_usage = false;
        for (deployment, usage) in &ledger.by_deployment {
            totals.add(usage);
            let cost_microusd = self.rates.get(deployment).map(|rate| {
                let records = ledger
                    .records
                    .iter()
                    .filter(|record| record.deployment == *deployment)
                    .collect::<Vec<_>>();
                let recorded = records
                    .iter()
                    .fold(TokenUsage::default(), |mut total, record| {
                        total.add(&record.usage);
                        total
                    });
                let baseline = usage.saturating_sub(&recorded);
                let numerator = records.into_iter().fold(
                    estimate_cost_numerator(&baseline, rate),
                    |total, record| {
                        total.saturating_add(estimate_cost_numerator(&record.usage, rate))
                    },
                );
                u64::try_from(numerator / TOKENS_PER_MILLION).unwrap_or(u64::MAX)
            });
            if let Some(cost) = cost_microusd {
                estimated_cost_microusd = estimated_cost_microusd.saturating_add(cost);
            } else if usage.total_tokens > 0 {
                has_unpriced_usage = true;
            }
            deployments.push(DeploymentUsageSnapshot {
                deployment: deployment.clone(),
                usage: usage.clone(),
                cost_microusd,
                pricing: self
                    .rates
                    .get(deployment)
                    .map(DeploymentPricing::rate_snapshot),
                long_context_pricing: self
                    .rates
                    .get(deployment)
                    .and_then(DeploymentPricing::long_context_snapshot),
                pricing_provenance: self
                    .rates
                    .get(deployment)
                    .map(|pricing| pricing.provenance().clone()),
            });
        }
        UsageSnapshot {
            usage: totals,
            last_response_tokens,
            estimated_cost_microusd,
            has_unpriced_usage,
            pricing_configured: self.is_configured(),
            deployments: Arc::from(deployments),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LiteLlmPricingEntry {
    #[serde(rename = "litellm_provider")]
    provider: Option<String>,
    input_cost_per_token: Option<f64>,
    cache_read_input_token_cost: Option<f64>,
    output_cost_per_token: Option<f64>,
}

fn catalog_provider_matches(selected: &str, catalog: Option<&str>) -> bool {
    let catalog = catalog.unwrap_or_default().trim().to_ascii_lowercase();
    match selected {
        "azure" | "openai" => matches!(catalog.as_str(), "openai" | "azure"),
        "aws-bedrock" | "bedrock-mantle" | "bedrock-runtime" => {
            matches!(catalog.as_str(), "bedrock" | "bedrock_converse")
        }
        "google" => matches!(catalog.as_str(), "gemini" | "vertex_ai" | "vertex_ai_beta"),
        "anthropic" => catalog == "anthropic",
        "nvidia" => matches!(catalog.as_str(), "nvidia" | "nvidia_nim"),
        "alibaba" => matches!(
            catalog.as_str(),
            "alibaba" | "dashscope" | "qwen" | "volcengine"
        ),
        "huggingface" => matches!(catalog.as_str(), "huggingface" | "hugging_face"),
        "github-models" => matches!(
            catalog.as_str(),
            "github" | "github_models" | "github_copilot"
        ),
        "compatible" => true,
        other => catalog == other.replace('-', "_") || catalog == other,
    }
}

pub fn load_remote_pricing_cache(
    path: &Path,
    provider: &str,
    catalog: &mut PricingCatalog,
) -> Result<usize, PricingRefreshError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(source) => {
            return Err(PricingRefreshError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(PricingRefreshError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pricing cache must be a regular non-symlink file",
            ),
        });
    }
    if metadata.len() > MAX_REMOTE_PRICING_BYTES as u64 {
        return Err(PricingRefreshError::TooLarge);
    }
    let file = fs::File::open(path).map_err(|source| PricingRefreshError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_REMOTE_PRICING_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| PricingRefreshError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_REMOTE_PRICING_BYTES {
        return Err(PricingRefreshError::TooLarge);
    }
    let updated_at = metadata
        .modified()
        .ok()
        .map(|modified| DateTime::<Utc>::from(modified).to_rfc3339_opts(SecondsFormat::Secs, true));
    catalog
        .merge_litellm_json_at(provider, &bytes, updated_at.as_deref())
        .map_err(Into::into)
}

pub async fn refresh_remote_pricing(
    url: &str,
    cache_path: &Path,
    provider: &str,
    timeout: Duration,
) -> Result<(PricingCatalog, usize), PricingRefreshError> {
    let url =
        Url::parse(url).map_err(|error| PricingRefreshError::InvalidUrl(error.to_string()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(PricingRefreshError::InvalidUrl(url.to_string()));
    }
    let bytes = download_remote_pricing(url, timeout).await?;
    let mut catalog = PricingCatalog::default();
    let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let imported = catalog.merge_litellm_json_at(provider, &bytes, Some(&updated_at))?;
    persist_remote_pricing_cache(cache_path, &bytes)?;
    Ok((catalog, imported))
}

async fn download_remote_pricing(
    url: Url,
    timeout: Duration,
) -> Result<Vec<u8>, PricingRefreshError> {
    tokio::time::timeout(timeout, download_remote_pricing_inner(url))
        .await
        .map_err(|_| PricingRefreshError::Timeout)?
}

async fn download_remote_pricing_inner(url: Url) -> Result<Vec<u8>, PricingRefreshError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .use_rustls_tls()
        .build()?;
    let response = client
        .get(url)
        .header("accept", "application/json")
        .header("user-agent", "decode-pricing-catalog/1")
        .send()
        .await?;
    if response.status() != StatusCode::OK {
        return Err(PricingRefreshError::Http(response.status().as_u16()));
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(PricingRefreshError::ContentType(content_type.to_owned()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REMOTE_PRICING_BYTES as u64)
    {
        return Err(PricingRefreshError::TooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_REMOTE_PRICING_BYTES {
            return Err(PricingRefreshError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn persist_remote_pricing_cache(path: &Path, bytes: &[u8]) -> Result<(), PricingRefreshError> {
    let parent = path
        .parent()
        .ok_or_else(|| PricingRefreshError::MissingParent(path.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| PricingRefreshError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| PricingRefreshError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| PricingRefreshError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| PricingRefreshError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PricingOverrideStore {
    path: PathBuf,
    rates: BTreeMap<String, DeploymentPricing>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPricingOverrides {
    version: u32,
    #[serde(default)]
    rates: Vec<StoredPricingRate>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredPricingRate {
    deployment: String,
    input_microusd_per_million: u64,
    cached_input_microusd_per_million: u64,
    output_microusd_per_million: u64,
    #[serde(default)]
    long_context_threshold_tokens: Option<u64>,
    #[serde(default)]
    long_context_input_microusd_per_million: Option<u64>,
    #[serde(default)]
    long_context_cached_input_microusd_per_million: Option<u64>,
    #[serde(default)]
    long_context_output_microusd_per_million: Option<u64>,
}

impl PricingOverrideStore {
    #[must_use]
    pub fn empty(path: PathBuf) -> Self {
        Self {
            path,
            rates: BTreeMap::new(),
        }
    }

    pub fn load(path: PathBuf) -> Result<Self, PricingStoreError> {
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::empty(path));
            }
            Err(source) => {
                return Err(PricingStoreError::Io {
                    path: path.clone(),
                    source,
                });
            }
        };
        if metadata.file_type().is_symlink() {
            return Err(PricingStoreError::Symlink(path));
        }
        if !metadata.is_file() {
            return Err(PricingStoreError::NotFile(path));
        }
        if metadata.len() > MAX_PRICING_OVERRIDES_BYTES {
            return Err(PricingStoreError::TooLarge(path));
        }
        let text = fs::read_to_string(&path).map_err(|source| PricingStoreError::Io {
            path: path.clone(),
            source,
        })?;
        let stored = toml::from_str::<StoredPricingOverrides>(&text).map_err(|error| {
            PricingStoreError::Parse {
                path: path.clone(),
                message: error.to_string(),
            }
        })?;
        if stored.version != PRICING_OVERRIDES_VERSION {
            return Err(PricingStoreError::UnsupportedVersion {
                found: stored.version,
            });
        }
        if stored.rates.len() > MAX_PRICING_OVERRIDES {
            return Err(PricingStoreError::TooManyEntries);
        }
        let mut rates = BTreeMap::new();
        for stored_rate in stored.rates {
            let pricing = DeploymentPricing::from_usd_per_million(
                stored_rate.deployment.clone(),
                stored_rate.input_microusd_per_million as f64 / MICROS_PER_USD as f64,
                Some(stored_rate.cached_input_microusd_per_million as f64 / MICROS_PER_USD as f64),
                stored_rate.output_microusd_per_million as f64 / MICROS_PER_USD as f64,
            )?;
            let pricing = match (
                stored_rate.long_context_threshold_tokens,
                stored_rate.long_context_input_microusd_per_million,
                stored_rate.long_context_output_microusd_per_million,
            ) {
                (None, None, None)
                    if stored_rate
                        .long_context_cached_input_microusd_per_million
                        .is_none() =>
                {
                    pricing
                }
                (Some(threshold), Some(input), Some(output)) => pricing.with_long_context_tier(
                    threshold,
                    input as f64 / MICROS_PER_USD as f64,
                    stored_rate
                        .long_context_cached_input_microusd_per_million
                        .map(|value| value as f64 / MICROS_PER_USD as f64),
                    output as f64 / MICROS_PER_USD as f64,
                )?,
                _ => return Err(PricingError::InvalidLongContextThreshold.into()),
            }
            .as_user_override();
            if rates
                .insert(stored_rate.deployment.clone(), pricing)
                .is_some()
            {
                return Err(PricingError::DuplicateDeployment(stored_rate.deployment).into());
            }
        }
        Ok(Self { path, rates })
    }

    pub fn rates(&self) -> impl Iterator<Item = &DeploymentPricing> {
        self.rates.values()
    }

    pub fn set(&mut self, pricing: DeploymentPricing) -> Result<(), PricingStoreError> {
        let pricing = pricing.as_user_override();
        let mut next = self.rates.clone();
        next.insert(pricing.deployment.clone(), pricing);
        if next.len() > MAX_PRICING_OVERRIDES {
            return Err(PricingStoreError::TooManyEntries);
        }
        self.persist_rates(&next)?;
        self.rates = next;
        Ok(())
    }

    pub fn remove(&mut self, deployment: &str) -> Result<bool, PricingStoreError> {
        let mut next = self.rates.clone();
        if next.remove(deployment).is_none() {
            return Ok(false);
        }
        self.persist_rates(&next)?;
        self.rates = next;
        Ok(true)
    }

    fn persist_rates(
        &self,
        rates: &BTreeMap<String, DeploymentPricing>,
    ) -> Result<(), PricingStoreError> {
        let stored = StoredPricingOverrides {
            version: PRICING_OVERRIDES_VERSION,
            rates: rates
                .values()
                .map(|rate| StoredPricingRate {
                    deployment: rate.deployment.clone(),
                    input_microusd_per_million: rate.input_microusd_per_million,
                    cached_input_microusd_per_million: rate.cached_input_microusd_per_million,
                    output_microusd_per_million: rate.output_microusd_per_million,
                    long_context_threshold_tokens: rate
                        .long_context
                        .as_ref()
                        .map(|(value, _)| *value),
                    long_context_input_microusd_per_million: rate
                        .long_context
                        .as_ref()
                        .map(|(_, tier)| tier.input_microusd_per_million),
                    long_context_cached_input_microusd_per_million: rate
                        .long_context
                        .as_ref()
                        .map(|(_, tier)| tier.cached_input_microusd_per_million),
                    long_context_output_microusd_per_million: rate
                        .long_context
                        .as_ref()
                        .map(|(_, tier)| tier.output_microusd_per_million),
                })
                .collect(),
        };
        let text = toml::to_string_pretty(&stored).map_err(|error| PricingStoreError::Parse {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        atomic_write(&self.path, text.as_bytes())?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PricingStoreError> {
    let parent = path
        .parent()
        .ok_or_else(|| PricingStoreError::MissingParent(path.to_path_buf()))?;
    fs::create_dir_all(parent).map_err(|source| PricingStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(PricingStoreError::Symlink(path.to_path_buf()));
    }
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| PricingStoreError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| PricingStoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| PricingStoreError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
    }

    fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            cached_input_tokens: self
                .cached_input_tokens
                .saturating_sub(other.cached_input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            total_tokens: self.total_tokens.saturating_sub(other.total_tokens),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct UsageRecord {
    deployment: String,
    usage: TokenUsage,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageLedger {
    by_deployment: BTreeMap<String, TokenUsage>,
    #[serde(default)]
    records: Vec<UsageRecord>,
}

impl UsageLedger {
    pub fn record(
        &mut self,
        deployment: &str,
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
    ) {
        let record = TokenUsage {
            input_tokens,
            cached_input_tokens: cached_input_tokens.min(input_tokens),
            output_tokens,
            total_tokens: total_tokens.max(input_tokens.saturating_add(output_tokens)),
        };
        let usage = self.by_deployment.entry(deployment.to_owned()).or_default();
        usage.add(&record);
        self.records.push(UsageRecord {
            deployment: deployment.to_owned(),
            usage: record,
        });
    }

    pub fn clear(&mut self) {
        self.by_deployment.clear();
        self.records.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_deployment.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentUsageSnapshot {
    pub deployment: String,
    pub usage: TokenUsage,
    pub cost_microusd: Option<u64>,
    pub pricing: Option<PricingRateSnapshot>,
    pub long_context_pricing: Option<LongContextPricingSnapshot>,
    pub pricing_provenance: Option<PricingProvenance>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageSnapshot {
    pub usage: TokenUsage,
    pub last_response_tokens: Option<u64>,
    pub estimated_cost_microusd: u64,
    pub has_unpriced_usage: bool,
    pub pricing_configured: bool,
    pub deployments: Arc<[DeploymentUsageSnapshot]>,
}

/// Describes how completely the configured tariff catalog covers the usage
/// that was actually reported by the API. A catalog can contain rates while
/// still not contain a rate for the active Azure deployment, so callers must
/// not use `pricing_configured` alone to decide whether `$0` is meaningful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostCoverage {
    NoUsage,
    Unpriced,
    Partial,
    Complete,
}

impl UsageSnapshot {
    #[must_use]
    pub fn cost_coverage(&self) -> CostCoverage {
        if self.usage.total_tokens == 0 {
            return CostCoverage::NoUsage;
        }
        let has_priced_usage = self
            .deployments
            .iter()
            .any(|item| item.usage.total_tokens > 0 && item.cost_microusd.is_some());
        match (has_priced_usage, self.has_unpriced_usage) {
            (false, true) => CostCoverage::Unpriced,
            (true, true) => CostCoverage::Partial,
            (_, false) => CostCoverage::Complete,
        }
    }
}

#[must_use]
pub fn format_microusd(value: u64) -> String {
    format!("${}.{:06}", value / MICROS_PER_USD, value % MICROS_PER_USD)
}

fn rate_to_microusd(field: &'static str, value: f64) -> Result<u64, PricingError> {
    if !value.is_finite() || !(0.0..=MAX_RATE_USD_PER_MILLION).contains(&value) {
        return Err(PricingError::InvalidRate { field });
    }
    Ok((value * MICROS_PER_USD as f64).round() as u64)
}

fn estimate_cost_numerator(usage: &TokenUsage, rate: &DeploymentPricing) -> u128 {
    let tier = rate
        .long_context
        .as_ref()
        .filter(|(threshold, _)| usage.input_tokens > *threshold)
        .map(|(_, tier)| tier);
    let input_rate = tier.map_or(rate.input_microusd_per_million, |tier| {
        tier.input_microusd_per_million
    });
    let cached_rate = tier.map_or(rate.cached_input_microusd_per_million, |tier| {
        tier.cached_input_microusd_per_million
    });
    let output_rate = tier.map_or(rate.output_microusd_per_million, |tier| {
        tier.output_microusd_per_million
    });
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let ordinary = usage.input_tokens.saturating_sub(cached);
    u128::from(ordinary)
        .saturating_mul(u128::from(input_rate))
        .saturating_add(u128::from(cached).saturating_mul(u128::from(cached_rate)))
        .saturating_add(u128::from(usage.output_tokens).saturating_mul(u128::from(output_rate)))
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    #[test]
    fn fixed_point_cost_accounts_for_cached_input_without_floats_at_runtime()
    -> Result<(), PricingError> {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "coding-prod".to_owned(),
            2.0,
            Some(0.5),
            8.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("coding-prod", 1_000_000, 250_000, 500_000, 1_500_000);
        let snapshot = catalog.snapshot(&ledger, Some(1_500_000));
        assert_eq!(snapshot.estimated_cost_microusd, 5_625_000);
        assert_eq!(
            format_microusd(snapshot.estimated_cost_microusd),
            "$5.625000"
        );
        assert!(!snapshot.has_unpriced_usage);
        Ok(())
    }

    #[test]
    fn unknown_deployment_is_explicitly_unpriced_instead_of_zero_cost() -> Result<(), PricingError>
    {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "known".to_owned(),
            1.0,
            None,
            2.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("custom-azure-name", 10, 0, 5, 15);
        let snapshot = catalog.snapshot(&ledger, Some(15));
        assert!(snapshot.has_unpriced_usage);
        assert_eq!(snapshot.deployments[0].cost_microusd, None);
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Unpriced);
        Ok(())
    }

    #[test]
    fn catalog_entry_for_another_deployment_never_turns_unpriced_usage_into_zero_cost()
    -> Result<(), PricingError> {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "placeholder".to_owned(),
            1.0,
            None,
            2.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("gpt-5.6-sol", 4_793, 0, 118, 4_911);

        let snapshot = catalog.snapshot(&ledger, Some(4_911));

        assert!(snapshot.pricing_configured);
        assert_eq!(snapshot.estimated_cost_microusd, 0);
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Unpriced);
        Ok(())
    }

    #[test]
    fn mixed_catalog_coverage_is_partial() -> Result<(), PricingError> {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "priced".to_owned(),
            1.0,
            None,
            2.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("priced", 1_000, 0, 100, 1_100);
        ledger.record("unpriced", 1_000, 0, 100, 1_100);

        let snapshot = catalog.snapshot(&ledger, Some(1_100));

        assert_eq!(snapshot.cost_coverage(), CostCoverage::Partial);
        assert!(snapshot.estimated_cost_microusd > 0);
        Ok(())
    }

    #[test]
    fn long_context_tariff_is_selected_per_response_not_from_session_aggregate()
    -> Result<(), PricingError> {
        let rate =
            DeploymentPricing::from_usd_per_million("tiered".to_owned(), 1.0, Some(0.1), 2.0)?
                .with_long_context_tier(200_000, 4.0, Some(0.4), 8.0)?;
        let catalog = PricingCatalog::new(vec![rate])?;

        let mut two_short = UsageLedger::default();
        two_short.record("tiered", 150_000, 0, 10_000, 160_000);
        two_short.record("tiered", 150_000, 0, 10_000, 160_000);
        let short_cost = catalog.snapshot(&two_short, Some(160_000));
        assert_eq!(short_cost.estimated_cost_microusd, 340_000);

        let mut one_long = UsageLedger::default();
        one_long.record("tiered", 300_000, 0, 20_000, 320_000);
        let long_cost = catalog.snapshot(&one_long, Some(320_000));
        assert_eq!(long_cost.estimated_cost_microusd, 1_360_000);
        Ok(())
    }

    #[test]
    fn duplicate_and_non_finite_rates_fail_closed() -> Result<(), PricingError> {
        let rate = DeploymentPricing::from_usd_per_million("same".to_owned(), 1.0, None, 2.0)?;
        assert!(matches!(
            PricingCatalog::new(vec![rate.clone(), rate]),
            Err(PricingError::DuplicateDeployment(_))
        ));
        assert!(matches!(
            DeploymentPricing::from_usd_per_million("bad".to_owned(), f64::NAN, None, 2.0),
            Err(PricingError::InvalidRate { .. })
        ));
        Ok(())
    }

    #[test]
    fn deployment_name_requires_visible_text() {
        assert!(matches!(
            DeploymentPricing::from_usd_per_million("\u{200b}\u{200d}".to_owned(), 1.0, None, 2.0),
            Err(PricingError::InvalidDeployment)
        ));
    }

    #[test]
    fn fractional_microusd_is_accumulated_across_responses() -> Result<(), PricingError> {
        let catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "small".to_owned(),
            0.5,
            None,
            0.0,
        )?])?;
        let mut ledger = UsageLedger::default();
        ledger.record("small", 1, 0, 0, 1);
        ledger.record("small", 1, 0, 0, 1);

        assert_eq!(
            catalog.snapshot(&ledger, Some(1)).estimated_cost_microusd,
            1
        );
        Ok(())
    }

    #[test]
    fn inconsistent_provider_total_cannot_hide_reported_usage() {
        let mut ledger = UsageLedger::default();
        ledger.record("unpriced", 10, 0, 5, 0);

        let snapshot = PricingCatalog::default().snapshot(&ledger, Some(0));
        assert_eq!(snapshot.usage.total_tokens, 15);
        assert!(snapshot.has_unpriced_usage);
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Unpriced);
    }

    #[test]
    fn user_override_round_trips_atomically_and_reprices_existing_usage()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("pricing-overrides-azure.toml");
        let mut store = PricingOverrideStore::load(path.clone())?;
        let pricing = DeploymentPricing::from_usd_per_million(
            "private-deployment".to_owned(),
            3.25,
            Some(0.75),
            12.5,
        )?;
        store.set(pricing)?;

        let restored = PricingOverrideStore::load(path)?;
        let mut catalog = PricingCatalog::default();
        for rate in restored.rates() {
            catalog.upsert(rate.clone());
        }
        let mut ledger = UsageLedger::default();
        ledger.record("private-deployment", 1_000_000, 200_000, 100_000, 1_100_000);
        let snapshot = catalog.snapshot(&ledger, Some(1_100_000));

        assert_eq!(snapshot.estimated_cost_microusd, 4_000_000);
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Complete);
        assert_eq!(
            snapshot.deployments[0]
                .pricing
                .map(PricingRateSnapshot::output_usd_per_million),
            Some(12.5)
        );
        Ok(())
    }

    #[test]
    fn malformed_or_unsupported_override_files_fail_without_partial_rates()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let malformed = directory.path().join("malformed.toml");
        fs::write(&malformed, "version = [")?;
        assert!(matches!(
            PricingOverrideStore::load(malformed),
            Err(PricingStoreError::Parse { .. })
        ));

        let unsupported = directory.path().join("unsupported.toml");
        fs::write(&unsupported, "version = 99\nrates = []\n")?;
        assert!(matches!(
            PricingOverrideStore::load(unsupported),
            Err(PricingStoreError::UnsupportedVersion { found: 99 })
        ));
        Ok(())
    }

    #[test]
    fn oversized_remote_cache_is_rejected_before_parsing() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("pricing-cache.json");
        fs::write(&path, vec![b' '; MAX_REMOTE_PRICING_BYTES + 1])?;
        let mut catalog = PricingCatalog::default();

        assert!(matches!(
            load_remote_pricing_cache(&path, "openai", &mut catalog),
            Err(PricingRefreshError::TooLarge)
        ));
        assert!(!catalog.is_configured());
        Ok(())
    }

    #[test]
    fn litellm_catalog_import_is_provider_scoped_and_exact() -> Result<(), PricingError> {
        let bytes = br#"{
            "openai/gpt-test": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.000002,
                "cache_read_input_token_cost": 0.0000002,
                "output_cost_per_token": 0.00001
            },
            "anthropic/claude-test": {
                "litellm_provider": "anthropic",
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000015
            }
        }"#;
        let mut catalog = PricingCatalog::default();
        assert_eq!(catalog.merge_litellm_json("openai", bytes)?, 1);
        let mut ledger = UsageLedger::default();
        ledger.record("gpt-test", 1_000_000, 500_000, 100_000, 1_100_000);
        ledger.record("claude-test", 100, 0, 100, 200);
        let snapshot = catalog.snapshot(&ledger, Some(200));
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Partial);
        assert_eq!(snapshot.deployments[0].cost_microusd, None);
        assert_eq!(snapshot.deployments[1].cost_microusd, Some(2_100_000));
        let provenance = snapshot.deployments[1].pricing_provenance.as_ref().ok_or(
            PricingError::InvalidCatalog("missing imported provenance".to_owned()),
        )?;
        assert!(provenance.is_approximate());
        Ok(())
    }

    #[test]
    fn lower_priority_public_catalog_never_overwrites_exact_configuration()
    -> Result<(), PricingError> {
        let exact =
            DeploymentPricing::from_usd_per_million("gpt-test".to_owned(), 7.0, Some(1.0), 19.0)?;
        let mut catalog = PricingCatalog::new(vec![exact])?;
        let bytes = br#"{
            "openai/gpt-test": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.00001
            }
        }"#;
        assert_eq!(catalog.merge_litellm_json("openai", bytes)?, 1);
        let mut ledger = UsageLedger::default();
        ledger.record("gpt-test", 1_000_000, 0, 0, 1_000_000);
        let snapshot = catalog.snapshot(&ledger, Some(1_000_000));
        assert_eq!(snapshot.estimated_cost_microusd, 7_000_000);
        assert_eq!(
            snapshot.deployments[0]
                .pricing_provenance
                .as_ref()
                .map(|source| source.source),
            Some(PricingSource::Configuration)
        );
        Ok(())
    }

    #[test]
    fn catalog_merge_is_transactional_when_a_later_rate_is_invalid() -> Result<(), PricingError> {
        let mut catalog = PricingCatalog::new(vec![DeploymentPricing::from_usd_per_million(
            "configured".to_owned(),
            1.0,
            None,
            2.0,
        )?])?;
        let before = catalog.clone();
        let bytes = br#"{
            "a-valid": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.00001
            },
            "z-invalid": {
                "litellm_provider": "openai",
                "input_cost_per_token": -0.000001,
                "output_cost_per_token": 0.00001
            }
        }"#;

        assert!(catalog.merge_litellm_json("openai", bytes).is_err());
        assert_eq!(catalog, before);
        Ok(())
    }

    #[test]
    fn bedrock_provider_ids_import_bedrock_catalog_entries() -> Result<(), PricingError> {
        let bytes = br#"{
            "bedrock/model": {
                "litellm_provider": "bedrock",
                "input_cost_per_token": 0.000002,
                "output_cost_per_token": 0.00001
            }
        }"#;

        for provider in ["bedrock-mantle", "bedrock-runtime"] {
            let mut catalog = PricingCatalog::default();
            assert_eq!(catalog.merge_litellm_json(provider, bytes)?, 1);
        }
        Ok(())
    }

    #[test]
    fn ambiguous_unqualified_catalog_alias_is_not_priced_arbitrarily() -> Result<(), PricingError> {
        let bytes = br#"{
            "azure/shared": {
                "litellm_provider": "azure",
                "input_cost_per_token": 0.000001,
                "output_cost_per_token": 0.000002
            },
            "openai/shared": {
                "litellm_provider": "openai",
                "input_cost_per_token": 0.000003,
                "output_cost_per_token": 0.000004
            }
        }"#;
        let mut catalog = PricingCatalog::default();
        assert_eq!(catalog.merge_litellm_json("openai", bytes)?, 2);
        let mut ledger = UsageLedger::default();
        ledger.record("shared", 1_000_000, 0, 0, 1_000_000);

        let snapshot = catalog.snapshot(&ledger, Some(1_000_000));
        assert_eq!(snapshot.cost_coverage(), CostCoverage::Unpriced);
        Ok(())
    }

    #[test]
    fn long_context_override_round_trips_and_can_be_removed_atomically()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("pricing-overrides-openai.toml");
        let pricing =
            DeploymentPricing::from_usd_per_million("custom".to_owned(), 1.0, Some(0.1), 5.0)?
                .with_long_context_tier(200_000, 2.0, Some(0.2), 8.0)?;
        let mut store = PricingOverrideStore::empty(path.clone());
        store.set(pricing)?;

        let mut restored = PricingOverrideStore::load(path.clone())?;
        let rate = restored
            .rates()
            .next()
            .ok_or_else(|| std::io::Error::other("missing restored rate"))?;
        assert_eq!(
            rate.long_context_snapshot()
                .map(|long| long.threshold_tokens),
            Some(200_000)
        );
        assert_eq!(rate.provenance.source, PricingSource::UserOverride);
        assert!(restored.remove("custom")?);
        assert!(!restored.remove("custom")?);
        assert_eq!(PricingOverrideStore::load(path)?.rates().count(), 0);
        Ok(())
    }

    #[test]
    fn catalog_provider_aliases_cover_named_adapters() {
        assert!(catalog_provider_matches("nvidia", Some("nvidia_nim")));
        assert!(catalog_provider_matches("alibaba", Some("dashscope")));
        assert!(catalog_provider_matches(
            "github-models",
            Some("github_models")
        ));
        assert!(!catalog_provider_matches("openai", Some("anthropic")));
    }

    #[tokio::test]
    async fn remote_catalog_is_https_only() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let result = refresh_remote_pricing(
            "http://example.test/prices.json",
            &directory.path().join("prices.json"),
            "openai",
            Duration::from_secs(1),
        )
        .await;
        assert!(matches!(result, Err(PricingRefreshError::InvalidUrl(_))));
        Ok(())
    }

    #[tokio::test]
    async fn remote_catalog_url_rejects_credentials_and_fragments()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        for url in [
            "https://user:secret@127.0.0.1/prices.json",
            "https://127.0.0.1/prices.json#ignored",
        ] {
            let result = refresh_remote_pricing(
                url,
                &directory.path().join("prices.json"),
                "openai",
                Duration::ZERO,
            )
            .await;
            assert!(matches!(result, Err(PricingRefreshError::InvalidUrl(_))));
        }
        Ok(())
    }

    #[tokio::test]
    async fn remote_catalog_timeout_covers_the_response_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await?;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n",
                )
                .await?;
            stream.flush().await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            stream.write_all(b"0\r\n\r\n").await?;
            Ok::<_, std::io::Error>(())
        });

        let started = std::time::Instant::now();
        let result = download_remote_pricing(
            Url::parse(&format!("http://{address}/prices.json"))?,
            Duration::from_millis(50),
        )
        .await;
        assert!(matches!(result, Err(PricingRefreshError::Timeout)));
        assert!(started.elapsed() < Duration::from_millis(300));
        server.abort();
        Ok(())
    }
}
