use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Cursor, Read, Write},
    net::IpAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;
use url::Url;
use zip::ZipArchive;

use futures_util::StreamExt as _;

pub const PLUGIN_MANIFEST_FILE: &str = "plugin.json";
const REGISTRY_FILE: &str = "registry.json";
const PACKAGES_DIR: &str = "packages";
const MAX_PACKAGE_BYTES: usize = 32 * 1024 * 1024;
const MAX_UNPACKED_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILES: usize = 1_024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_MARKETPLACE_BYTES: usize = 4 * 1024 * 1024;
const MAX_MARKETPLACES: usize = 16;
const MAX_MARKETPLACE_ENTRIES: usize = 2_048;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub components: PluginComponents,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginComponents {
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
    #[serde(default)]
    pub lsp: Vec<String>,
    #[serde(default)]
    pub assets: Vec<String>,
}

impl PluginComponents {
    fn all_paths(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.skills
            .iter()
            .map(|path| ("skills", path.as_str()))
            .chain(self.commands.iter().map(|path| ("commands", path.as_str())))
            .chain(self.profiles.iter().map(|path| ("profiles", path.as_str())))
            .chain(self.hooks.iter().map(|path| ("hooks", path.as_str())))
            .chain(self.mcp.iter().map(|path| ("mcp", path.as_str())))
            .chain(self.lsp.iter().map(|path| ("lsp", path.as_str())))
            .chain(self.assets.iter().map(|path| ("assets", path.as_str())))
    }

    #[must_use]
    pub fn labels(&self) -> Arc<[String]> {
        let mut labels = Vec::new();
        for (label, count) in [
            ("skills", self.skills.len()),
            ("commands", self.commands.len()),
            ("profiles", self.profiles.len()),
            ("hooks", self.hooks.len()),
            ("MCP", self.mcp.len()),
            ("LSP", self.lsp.len()),
            ("assets", self.assets.len()),
        ] {
            if count > 0 {
                labels.push(format!("{label} ×{count}"));
            }
        }
        labels.into()
    }

    #[must_use]
    pub fn has_privileged_components(&self) -> bool {
        !self.hooks.is_empty() || !self.mcp.is_empty() || !self.lsp.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketplaceDocument {
    pub schema_version: u32,
    #[serde(default)]
    pub name: String,
    pub plugins: Vec<MarketplacePlugin>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MarketplacePlugin {
    pub id: String,
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub description: String,
    pub package_url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct InstalledRecord {
    manifest: PluginManifest,
    enabled: bool,
    source: String,
    package_sha256: String,
    #[serde(default)]
    activated_paths: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Registry {
    schema_version: u32,
    #[serde(default)]
    marketplaces: Vec<String>,
    #[serde(default)]
    installed: BTreeMap<String, InstalledRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: Version,
    pub description: String,
    pub publisher: String,
    pub enabled: bool,
    pub source: String,
    pub components: Arc<[String]>,
    pub privileged: bool,
    pub update: Option<Version>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceSummary {
    pub source: String,
    pub name: String,
    pub plugins: Arc<[MarketplacePlugin]>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginSnapshot {
    pub revision: u64,
    pub root: PathBuf,
    pub plugins: Arc<[PluginSummary]>,
    pub marketplaces: Arc<[MarketplaceSummary]>,
    pub diagnostics: Arc<[String]>,
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin storage I/O failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin registry is invalid: {0}")]
    Registry(String),
    #[error("plugin package is invalid: {0}")]
    InvalidPackage(String),
    #[error("plugin manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("plugin {0:?} is not installed")]
    UnknownPlugin(String),
    #[error("plugin {id:?} version {version} is already installed")]
    AlreadyInstalled { id: String, version: Version },
    #[error("plugin package digest mismatch: expected {expected}, got {actual}")]
    DigestMismatch { expected: String, actual: String },
    #[error("marketplace source is invalid: {0}")]
    InvalidMarketplace(String),
    #[error("marketplace request failed: {0}")]
    Network(String),
    #[error("marketplace has no package for plugin {0:?}")]
    MarketplaceEntryMissing(String),
    #[error("plugin storage path must be a real directory: {0}")]
    UnsafeStorage(PathBuf),
}

#[derive(Debug)]
pub struct PluginManager {
    root: PathBuf,
    integration_root: PathBuf,
    registry: Registry,
    revision: u64,
    marketplaces: Vec<MarketplaceSummary>,
    diagnostics: Vec<String>,
    client: reqwest::Client,
}

impl PluginManager {
    pub fn open(
        root: PathBuf,
        integration_root: PathBuf,
        timeout: Duration,
    ) -> Result<Self, PluginError> {
        ensure_real_directory(&root)?;
        ensure_real_directory(&root.join(PACKAGES_DIR))?;
        ensure_real_directory(&integration_root)?;
        let registry_path = root.join(REGISTRY_FILE);
        let registry = match fs::read(&registry_path) {
            Ok(bytes) => serde_json::from_slice::<Registry>(&bytes)
                .map_err(|error| PluginError::Registry(error.to_string()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Registry {
                schema_version: 1,
                marketplaces: Vec::new(),
                installed: BTreeMap::new(),
            },
            Err(source) => {
                return Err(PluginError::Io {
                    path: registry_path,
                    source,
                });
            }
        };
        if registry.schema_version != 1 {
            return Err(PluginError::Registry(format!(
                "unsupported schema_version {}",
                registry.schema_version
            )));
        }
        if registry.marketplaces.len() > MAX_MARKETPLACES {
            return Err(PluginError::Registry(format!(
                "more than {MAX_MARKETPLACES} marketplaces"
            )));
        }
        for record in registry.installed.values() {
            validate_manifest(&record.manifest)?;
        }
        validate_registry(&registry)?;
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                if attempt.previous().len() >= 3 {
                    return attempt.error("too many marketplace redirects");
                }
                if validate_remote_url(attempt.url().as_str()).is_err() {
                    return attempt.error("marketplace redirect target is not allowed");
                }
                attempt.follow()
            }))
            .build()
            .map_err(|error| PluginError::Network(error.to_string()))?;
        Ok(Self {
            root,
            integration_root,
            registry,
            revision: 1,
            marketplaces: Vec::new(),
            diagnostics: Vec::new(),
            client,
        })
    }

    #[must_use]
    pub fn snapshot(&self) -> PluginSnapshot {
        let updates = self
            .marketplaces
            .iter()
            .flat_map(|marketplace| marketplace.plugins.iter())
            .fold(
                BTreeMap::<String, &Version>::new(),
                |mut versions, plugin| {
                    versions
                        .entry(plugin.id.to_ascii_lowercase())
                        .and_modify(|version| {
                            if plugin.version > **version {
                                *version = &plugin.version;
                            }
                        })
                        .or_insert(&plugin.version);
                    versions
                },
            );
        let plugins = self
            .registry
            .installed
            .values()
            .map(|record| {
                let update = updates
                    .get(&record.manifest.id.to_ascii_lowercase())
                    .filter(|version| ***version > record.manifest.version)
                    .map(|version| (**version).clone());
                PluginSummary {
                    id: record.manifest.id.clone(),
                    name: record.manifest.name.clone(),
                    version: record.manifest.version.clone(),
                    description: record.manifest.description.clone(),
                    publisher: record.manifest.publisher.clone(),
                    enabled: record.enabled,
                    source: record.source.clone(),
                    components: record.manifest.components.labels(),
                    privileged: record.manifest.components.has_privileged_components(),
                    update,
                }
            })
            .collect::<Vec<_>>();
        PluginSnapshot {
            revision: self.revision,
            root: self.root.clone(),
            plugins: plugins.into(),
            marketplaces: self.marketplaces.clone().into(),
            diagnostics: self.diagnostics.clone().into(),
        }
    }

    pub fn add_marketplace(&mut self, source: String) -> Result<(), PluginError> {
        validate_marketplace_source(&source)?;
        if self
            .registry
            .marketplaces
            .iter()
            .any(|item| item == &source)
        {
            return Ok(());
        }
        if self.registry.marketplaces.len() >= MAX_MARKETPLACES {
            return Err(PluginError::InvalidMarketplace(format!(
                "at most {MAX_MARKETPLACES} sources are allowed"
            )));
        }
        let mut registry = self.registry.clone();
        registry.marketplaces.push(source);
        self.persist_registry_value(&registry)?;
        self.registry = registry;
        self.bump();
        Ok(())
    }

    pub fn remove_marketplace(&mut self, source: &str) -> Result<(), PluginError> {
        let mut registry = self.registry.clone();
        registry.marketplaces.retain(|item| item != source);
        self.persist_registry_value(&registry)?;
        self.registry = registry;
        self.marketplaces.retain(|item| item.source != source);
        self.bump();
        Ok(())
    }

    pub async fn refresh_marketplaces(&mut self) {
        let sources = self.registry.marketplaces.clone();
        let mut results = Vec::with_capacity(sources.len());
        for source in sources {
            let result = self.load_marketplace(&source).await;
            results.push(match result {
                Ok(document) => MarketplaceSummary {
                    source,
                    name: document.name,
                    plugins: document.plugins.into(),
                    error: None,
                },
                Err(error) => MarketplaceSummary {
                    source,
                    name: String::new(),
                    plugins: Arc::from([]),
                    error: Some(error.to_string()),
                },
            });
        }
        self.marketplaces = results;
        self.bump();
    }

    pub async fn install_local(&mut self, package: &Path) -> Result<String, PluginError> {
        let bytes = read_bounded(package, MAX_PACKAGE_BYTES)?;
        let digest = sha256_hex(&bytes);
        self.install_bytes(bytes, package.display().to_string(), digest, false, None)
            .await
    }

    pub async fn install_marketplace(&mut self, plugin_id: &str) -> Result<String, PluginError> {
        let entry = self
            .marketplaces
            .iter()
            .flat_map(|marketplace| marketplace.plugins.iter())
            .filter(|plugin| plugin.id.eq_ignore_ascii_case(plugin_id))
            .max_by(|left, right| left.version.cmp(&right.version))
            .cloned()
            .ok_or_else(|| PluginError::MarketplaceEntryMissing(plugin_id.to_owned()))?;
        let bytes = self
            .download_bounded(&entry.package_url, MAX_PACKAGE_BYTES)
            .await?;
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(&entry.sha256) {
            return Err(PluginError::DigestMismatch {
                expected: entry.sha256,
                actual,
            });
        }
        let expected = Some((entry.id.clone(), entry.version.clone()));
        self.install_bytes(bytes, entry.package_url, actual, true, expected)
            .await
    }

    pub async fn update(&mut self, plugin_id: &str) -> Result<String, PluginError> {
        let was_enabled = self
            .registry
            .installed
            .get(plugin_id)
            .ok_or_else(|| PluginError::UnknownPlugin(plugin_id.to_owned()))?
            .enabled;
        let installed = self.install_marketplace(plugin_id).await?;
        if was_enabled {
            self.set_enabled(plugin_id, true)?;
        }
        Ok(installed)
    }

    pub fn set_enabled(&mut self, plugin_id: &str, enabled: bool) -> Result<(), PluginError> {
        let record = self
            .registry
            .installed
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(plugin_id.to_owned()))?;
        if record.enabled == enabled {
            return Ok(());
        }
        if enabled {
            let activated_paths = self.activate(&record)?;
            let mut updated = record.clone();
            updated.activated_paths.clone_from(&activated_paths);
            updated.enabled = true;
            let mut registry = self.registry.clone();
            registry.installed.insert(plugin_id.to_owned(), updated);
            if let Err(error) = self.persist_registry_value(&registry) {
                if let Err(cleanup_error) = self.remove_activated(&activated_paths) {
                    tracing::error!(%cleanup_error, "failed to roll back plugin activation");
                }
                return Err(error);
            }
            self.registry = registry;
        } else {
            let mut updated = record.clone();
            updated.activated_paths.clear();
            updated.enabled = false;
            let mut registry = self.registry.clone();
            registry.installed.insert(plugin_id.to_owned(), updated);
            self.persist_registry_value(&registry)?;
            self.registry = registry;
            self.remove_activated(&record.activated_paths)?;
        }
        self.bump();
        Ok(())
    }

    pub fn remove(&mut self, plugin_id: &str) -> Result<(), PluginError> {
        let record = self
            .registry
            .installed
            .get(plugin_id)
            .cloned()
            .ok_or_else(|| PluginError::UnknownPlugin(plugin_id.to_owned()))?;
        let mut registry = self.registry.clone();
        registry.installed.remove(plugin_id);
        self.persist_registry_value(&registry)?;
        self.registry = registry;
        self.remove_activated(&record.activated_paths)?;
        let package = package_dir(&self.root, &record.manifest);
        if package.exists() {
            fs::remove_dir_all(&package).map_err(|source| PluginError::Io {
                path: package,
                source,
            })?;
        }
        self.bump();
        Ok(())
    }

    async fn install_bytes(
        &mut self,
        bytes: Vec<u8>,
        source: String,
        package_sha256: String,
        replace_older: bool,
        expected: Option<(String, Version)>,
    ) -> Result<String, PluginError> {
        let prepared = tokio::task::spawn_blocking(move || inspect_package(bytes))
            .await
            .map_err(|error| PluginError::InvalidPackage(error.to_string()))??;
        if let Some((expected_id, expected_version)) = expected
            && (prepared.manifest.id != expected_id
                || prepared.manifest.version != expected_version)
        {
            return Err(PluginError::InvalidPackage(format!(
                "marketplace entry {expected_id} {expected_version} delivered {} {}",
                prepared.manifest.id, prepared.manifest.version
            )));
        }
        let existing_key = self
            .registry
            .installed
            .keys()
            .find(|id| id.eq_ignore_ascii_case(&prepared.manifest.id))
            .cloned();
        let existing = existing_key
            .as_ref()
            .and_then(|id| self.registry.installed.get(id))
            .cloned();
        if let Some(existing) = &existing {
            if existing.manifest.version == prepared.manifest.version {
                return Err(PluginError::AlreadyInstalled {
                    id: prepared.manifest.id,
                    version: prepared.manifest.version,
                });
            }
            if !replace_older || prepared.manifest.version <= existing.manifest.version {
                return Err(PluginError::InvalidPackage(format!(
                    "installed version {} is not older than package version {}",
                    existing.manifest.version, prepared.manifest.version
                )));
            }
        }
        let manifest = prepared.manifest.clone();
        let target = package_dir(&self.root, &manifest);
        match fs::symlink_metadata(&target) {
            Ok(_) => {
                return Err(PluginError::AlreadyInstalled {
                    id: manifest.id,
                    version: manifest.version,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(PluginError::Io {
                    path: target,
                    source,
                });
            }
        }
        let staging_parent = self.root.join(PACKAGES_DIR);
        let staging = tempfile::Builder::new()
            .prefix(".plugin-staging-")
            .tempdir_in(&staging_parent)
            .map_err(|source| PluginError::Io {
                path: staging_parent.clone(),
                source,
            })?;
        write_prepared_package(staging.path(), &prepared)?;
        let parent = target.parent().ok_or_else(|| {
            PluginError::InvalidPackage("package target has no parent directory".to_owned())
        })?;
        ensure_real_directory(parent)?;
        let staged_path = staging.keep();
        fs::rename(&staged_path, &target).map_err(|source| PluginError::Io {
            path: target.clone(),
            source,
        })?;

        let mut registry = self.registry.clone();
        if let Some(existing_key) = &existing_key {
            registry.installed.remove(existing_key);
        }
        registry.installed.insert(
            manifest.id.clone(),
            InstalledRecord {
                manifest: manifest.clone(),
                enabled: false,
                source,
                package_sha256,
                activated_paths: Vec::new(),
            },
        );
        if let Err(error) = self.persist_registry_value(&registry) {
            if let Err(cleanup_error) = fs::remove_dir_all(&target) {
                tracing::error!(path = %target.display(), %cleanup_error, "failed to roll back plugin package");
            }
            return Err(error);
        }
        self.registry = registry;
        if let Some(previous) = existing {
            self.remove_activated(&previous.activated_paths)?;
            let old_package = package_dir(&self.root, &previous.manifest);
            if old_package.exists() {
                fs::remove_dir_all(&old_package).map_err(|source| PluginError::Io {
                    path: old_package,
                    source,
                })?;
            }
        }
        self.bump();
        Ok(format!("Installed {} {}", manifest.name, manifest.version))
    }

    fn activate(&self, record: &InstalledRecord) -> Result<Vec<String>, PluginError> {
        let package = package_dir(&self.root, &record.manifest);
        let namespace = safe_namespace(&record.manifest.id);
        let mut pending = Vec::new();
        let mut targets = BTreeSet::new();
        for (kind, relative) in record.manifest.components.all_paths() {
            let source = package.join(relative);
            let target_relative = activation_path(kind, relative, &namespace)?;
            if !targets.insert(target_relative.clone()) {
                return Err(PluginError::InvalidManifest(format!(
                    "multiple components resolve to {}",
                    target_relative.display()
                )));
            }
            let target = self.integration_root.join(&target_relative);
            ensure_safe_parent_path(&self.integration_root, &target_relative)?;
            if fs::symlink_metadata(&target).is_ok() {
                return Err(PluginError::InvalidPackage(format!(
                    "activation target already exists: {}",
                    target.display()
                )));
            }
            pending.push((kind, source, target_relative, target));
        }

        let mut activated = Vec::new();
        for (kind, source, target_relative, target) in pending {
            activated.push(path_to_registry(&target_relative)?);
            if let Err(error) = copy_component(&source, &target, kind, &namespace) {
                if let Err(cleanup_error) = self.remove_activated(&activated) {
                    tracing::error!(%cleanup_error, "failed to roll back partial plugin activation");
                }
                return Err(error);
            }
        }
        Ok(activated)
    }

    fn remove_activated(&self, paths: &[String]) -> Result<(), PluginError> {
        for relative in paths {
            let relative_path = checked_relative(relative)?;
            ensure_safe_parent_path(&self.integration_root, relative_path)?;
            let target = self.integration_root.join(relative_path);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    fs::remove_dir_all(&target).map_err(|source| PluginError::Io {
                        path: target,
                        source,
                    })?;
                }
                Ok(_) => {
                    fs::remove_file(&target).map_err(|source| PluginError::Io {
                        path: target,
                        source,
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(source) => {
                    return Err(PluginError::Io {
                        path: target,
                        source,
                    });
                }
            }
        }
        Ok(())
    }

    async fn load_marketplace(&self, source: &str) -> Result<MarketplaceDocument, PluginError> {
        let bytes = if let Ok(url) = Url::parse(source) {
            self.download_bounded(url.as_str(), MAX_MARKETPLACE_BYTES)
                .await?
        } else {
            read_bounded(Path::new(source), MAX_MARKETPLACE_BYTES)?
        };
        let document: MarketplaceDocument = serde_json::from_slice(&bytes)
            .map_err(|error| PluginError::InvalidMarketplace(error.to_string()))?;
        validate_marketplace_document(&document)?;
        Ok(document)
    }

    async fn download_bounded(&self, url: &str, limit: usize) -> Result<Vec<u8>, PluginError> {
        validate_remote_url(url)?;
        let response = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|error| PluginError::Network(error.to_string()))?
            .error_for_status()
            .map_err(|error| PluginError::Network(error.to_string()))?;
        validate_remote_url(response.url().as_str())?;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(PluginError::Network(format!(
                "response exceeds {limit} bytes"
            )));
        }
        let capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or_default()
            .min(limit);
        let mut bytes = Vec::with_capacity(capacity);
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| PluginError::Network(error.to_string()))?;
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(PluginError::Network(format!(
                    "response exceeds {limit} bytes"
                )));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    fn persist_registry_value(&self, registry: &Registry) -> Result<(), PluginError> {
        let bytes = serde_json::to_vec_pretty(registry)
            .map_err(|error| PluginError::Registry(error.to_string()))?;
        atomic_write(&self.root.join(REGISTRY_FILE), &bytes)
    }

    fn bump(&mut self) {
        self.revision = self.revision.saturating_add(1);
    }
}

#[derive(Debug)]
struct PreparedPackage {
    manifest: PluginManifest,
    files: Vec<(PathBuf, Vec<u8>)>,
}

fn validate_registry(registry: &Registry) -> Result<(), PluginError> {
    let mut identities = BTreeSet::new();
    for (key, record) in &registry.installed {
        if key != &record.manifest.id {
            return Err(PluginError::Registry(format!(
                "installed key {key:?} does not match manifest id {:?}",
                record.manifest.id
            )));
        }
        if !identities.insert(key.to_ascii_lowercase()) {
            return Err(PluginError::Registry(format!(
                "duplicate case-insensitive plugin id {key:?}"
            )));
        }
        let namespace = safe_namespace(key);
        let expected = record
            .manifest
            .components
            .all_paths()
            .map(|(kind, path)| activation_path(kind, path, &namespace))
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|error| PluginError::Registry(error.to_string()))?;
        let mut actual = BTreeSet::new();
        for path in &record.activated_paths {
            let path = checked_relative(path)
                .map_err(|error| PluginError::Registry(error.to_string()))?
                .to_path_buf();
            if !actual.insert(path.clone()) || !expected.contains(&path) {
                return Err(PluginError::Registry(format!(
                    "plugin {key:?} does not own activated path {}",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

fn inspect_package(bytes: Vec<u8>) -> Result<PreparedPackage, PluginError> {
    if bytes.len() > MAX_PACKAGE_BYTES {
        return Err(PluginError::InvalidPackage(format!(
            "package exceeds {MAX_PACKAGE_BYTES} bytes"
        )));
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .map_err(|error| PluginError::InvalidPackage(error.to_string()))?;
    if archive.len() > MAX_FILES {
        return Err(PluginError::InvalidPackage(format!(
            "package has more than {MAX_FILES} entries"
        )));
    }
    let mut total = 0_u64;
    let mut seen = BTreeSet::new();
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| PluginError::InvalidPackage(error.to_string()))?;
        let relative = entry
            .enclosed_name()
            .ok_or_else(|| {
                PluginError::InvalidPackage(format!("unsafe archive entry {:?}", entry.name()))
            })?
            .to_path_buf();
        validate_relative_path(&relative)?;
        if entry.is_dir() {
            continue;
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            return Err(PluginError::InvalidPackage(format!(
                "symbolic link entry is forbidden: {}",
                relative.display()
            )));
        }
        if !seen.insert(relative.clone()) {
            return Err(PluginError::InvalidPackage(format!(
                "duplicate archive entry {}",
                relative.display()
            )));
        }
        total = total.saturating_add(entry.size());
        if total > MAX_UNPACKED_BYTES {
            return Err(PluginError::InvalidPackage(format!(
                "unpacked package exceeds {MAX_UNPACKED_BYTES} bytes"
            )));
        }
        let capacity = usize::try_from(entry.size()).map_err(|_| {
            PluginError::InvalidPackage("archive entry does not fit in memory".to_owned())
        })?;
        let mut contents = Vec::with_capacity(capacity);
        entry
            .read_to_end(&mut contents)
            .map_err(|source| PluginError::Io {
                path: relative.clone(),
                source,
            })?;
        files.push((relative, contents));
    }
    let manifest_bytes = files
        .iter()
        .find(|(path, _)| path == Path::new(PLUGIN_MANIFEST_FILE))
        .map(|(_, bytes)| bytes)
        .ok_or_else(|| {
            PluginError::InvalidPackage("plugin.json is missing at package root".to_owned())
        })?;
    if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(PluginError::InvalidPackage(
            "plugin.json is too large".to_owned(),
        ));
    }
    let manifest: PluginManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|error| PluginError::InvalidManifest(error.to_string()))?;
    validate_manifest(&manifest)?;
    for (_, component) in manifest.components.all_paths() {
        let component = checked_relative(component)?;
        let found = files
            .iter()
            .any(|(path, _)| path == component || path.starts_with(component));
        if !found {
            return Err(PluginError::InvalidManifest(format!(
                "declared component {:?} is missing from package",
                component.display()
            )));
        }
    }
    Ok(PreparedPackage { manifest, files })
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), PluginError> {
    if manifest.schema_version != 1 {
        return Err(PluginError::InvalidManifest(format!(
            "unsupported schema_version {}",
            manifest.schema_version
        )));
    }
    validate_identifier(&manifest.id, "id")?;
    validate_text(&manifest.name, "name", 128, false)?;
    validate_text(&manifest.description, "description", 4_096, true)?;
    validate_text(&manifest.publisher, "publisher", 256, true)?;
    if manifest.version == Version::new(0, 0, 0) {
        return Err(PluginError::InvalidManifest(
            "version 0.0.0 is reserved".to_owned(),
        ));
    }
    let mut paths = BTreeSet::new();
    for (kind, path) in manifest.components.all_paths() {
        let relative = checked_relative(path)?;
        if !paths.insert(relative.to_path_buf()) {
            return Err(PluginError::InvalidManifest(format!(
                "component path {path:?} is declared more than once"
            )));
        }
        match kind {
            "commands" | "profiles" | "hooks" | "mcp" | "lsp"
                if relative
                    .extension()
                    .is_none_or(|extension| extension != "toml") =>
            {
                return Err(PluginError::InvalidManifest(format!(
                    "{kind} component {path:?} must be TOML"
                )));
            }
            _ => {}
        }
    }
    if let Some(homepage) = &manifest.homepage {
        validate_remote_url(homepage)?;
    }
    Ok(())
}

fn validate_marketplace_document(document: &MarketplaceDocument) -> Result<(), PluginError> {
    if document.schema_version != 1 {
        return Err(PluginError::InvalidMarketplace(format!(
            "unsupported schema_version {}",
            document.schema_version
        )));
    }
    if document.plugins.len() > MAX_MARKETPLACE_ENTRIES {
        return Err(PluginError::InvalidMarketplace(format!(
            "more than {MAX_MARKETPLACE_ENTRIES} entries"
        )));
    }
    let mut identities = BTreeSet::new();
    for plugin in &document.plugins {
        validate_identifier(&plugin.id, "marketplace plugin id")?;
        validate_text(&plugin.name, "marketplace plugin name", 128, false)?;
        validate_remote_url(&plugin.package_url)?;
        if plugin.sha256.len() != 64 || !plugin.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(PluginError::InvalidMarketplace(format!(
                "plugin {:?} has an invalid SHA-256 digest",
                plugin.id
            )));
        }
        if !identities.insert((plugin.id.to_ascii_lowercase(), plugin.version.clone())) {
            return Err(PluginError::InvalidMarketplace(format!(
                "duplicate plugin/version {:?} {}",
                plugin.id, plugin.version
            )));
        }
    }
    Ok(())
}

fn validate_marketplace_source(source: &str) -> Result<(), PluginError> {
    if source.len() > 4_096 || source.contains(char::is_control) {
        return Err(PluginError::InvalidMarketplace(
            "source is empty, too long, or contains control characters".to_owned(),
        ));
    }
    if Url::parse(source).is_ok() {
        return validate_remote_url(source);
    }
    let path = Path::new(source);
    if !path.is_absolute() {
        return Err(PluginError::InvalidMarketplace(
            "local marketplace source must be an absolute path".to_owned(),
        ));
    }
    Ok(())
}

fn validate_remote_url(value: &str) -> Result<(), PluginError> {
    let url =
        Url::parse(value).map_err(|error| PluginError::InvalidMarketplace(error.to_string()))?;
    let loopback_http = url.scheme() == "http" && url.host_str().is_some_and(is_loopback_host);
    if url.scheme() != "https" && !loopback_http {
        return Err(PluginError::InvalidMarketplace(
            "remote URLs must use HTTPS; HTTP is allowed only for loopback development".to_owned(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(PluginError::InvalidMarketplace(
            "URL credentials and fragments are forbidden".to_owned(),
        ));
    }
    Ok(())
}

fn is_loopback_host(host: &str) -> bool {
    let host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_identifier(value: &str, field: &str) -> Result<(), PluginError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
        || !value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(PluginError::InvalidManifest(format!(
            "{field} must be 1-128 ASCII letters, digits, dots, dashes, or underscores and start with an alphanumeric character"
        )));
    }
    Ok(())
}

fn validate_text(
    value: &str,
    field: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), PluginError> {
    if (!allow_empty && value.trim().is_empty()) || value.len() > max_bytes || value.contains('\0')
    {
        return Err(PluginError::InvalidManifest(format!(
            "{field} is empty, too long, or contains NUL"
        )));
    }
    Ok(())
}

fn checked_relative(value: &str) -> Result<&Path, PluginError> {
    let path = Path::new(value);
    validate_relative_path(path)?;
    Ok(path)
}

fn validate_relative_path(path: &Path) -> Result<(), PluginError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || component.as_os_str().to_string_lossy().contains('\0')
        })
    {
        return Err(PluginError::InvalidPackage(format!(
            "unsafe relative path {:?}",
            path
        )));
    }
    Ok(())
}

fn safe_namespace(id: &str) -> String {
    id.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

fn activation_path(kind: &str, relative: &str, namespace: &str) -> Result<PathBuf, PluginError> {
    let leaf = Path::new(relative)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PluginError::InvalidManifest(format!("invalid component {relative:?}")))?;
    match kind {
        "skills" => Ok(PathBuf::from("skills").join(format!("plugin-{namespace}-{leaf}"))),
        "commands" => Ok(PathBuf::from("commands").join(format!("plugin-{namespace}-{leaf}"))),
        "profiles" => Ok(PathBuf::from("agents").join(format!("plugin-{namespace}-{leaf}"))),
        "hooks" => Ok(PathBuf::from("hooks").join(format!("plugin-{namespace}-{leaf}"))),
        "mcp" => Ok(PathBuf::from("plugin-connections")
            .join(namespace)
            .join("mcp")
            .join(leaf)),
        "lsp" => Ok(PathBuf::from("plugin-connections")
            .join(namespace)
            .join("lsp")
            .join(leaf)),
        "assets" => Ok(PathBuf::from("plugin-assets").join(namespace).join(leaf)),
        _ => Err(PluginError::InvalidManifest(format!(
            "unknown component type {kind}"
        ))),
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), PluginError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(PluginError::UnsafeStorage(path.to_path_buf()));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(PluginError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    fs::create_dir_all(path).map_err(|source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginError::UnsafeStorage(path.to_path_buf()));
    }
    Ok(())
}

fn ensure_safe_parent_path(root: &Path, relative: &Path) -> Result<(), PluginError> {
    validate_relative_path(relative)?;
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    let mut current = root.to_path_buf();
    for component in parent.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(PluginError::UnsafeStorage(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(source) => {
                return Err(PluginError::Io {
                    path: current,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn package_dir(root: &Path, manifest: &PluginManifest) -> PathBuf {
    root.join(PACKAGES_DIR)
        .join(safe_namespace(&manifest.id))
        .join(manifest.version.to_string())
}

fn write_prepared_package(root: &Path, package: &PreparedPackage) -> Result<(), PluginError> {
    for (relative, contents) in &package.files {
        let target = root.join(relative);
        let parent = target.parent().ok_or_else(|| {
            PluginError::InvalidPackage(format!("path {:?} has no parent", relative))
        })?;
        fs::create_dir_all(parent).map_err(|source| PluginError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        fs::write(&target, contents).map_err(|source| PluginError::Io {
            path: target,
            source,
        })?;
    }
    Ok(())
}

fn copy_component(
    source: &Path,
    target: &Path,
    kind: &str,
    namespace: &str,
) -> Result<(), PluginError> {
    let metadata = fs::symlink_metadata(source).map_err(|source_error| PluginError::Io {
        path: source.to_path_buf(),
        source: source_error,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(PluginError::InvalidPackage(format!(
            "component is a symbolic link: {}",
            source.display()
        )));
    }
    if metadata.is_dir() {
        fs::create_dir_all(target).map_err(|source_error| PluginError::Io {
            path: target.to_path_buf(),
            source: source_error,
        })?;
        let mut entries = fs::read_dir(source)
            .map_err(|source_error| PluginError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source_error| PluginError::Io {
                path: source.to_path_buf(),
                source: source_error,
            })?;
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            copy_component(
                &entry.path(),
                &target.join(entry.file_name()),
                kind,
                namespace,
            )?;
        }
        return Ok(());
    }
    let parent = target.parent().ok_or_else(|| {
        PluginError::InvalidPackage(format!("target {} has no parent", target.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source_error| PluginError::Io {
        path: parent.to_path_buf(),
        source: source_error,
    })?;
    if matches!(kind, "commands" | "profiles" | "hooks") {
        let bytes = read_bounded(source, 512 * 1024)?;
        let text = String::from_utf8(bytes).map_err(|error| {
            PluginError::InvalidPackage(format!("{} is not UTF-8: {error}", source.display()))
        })?;
        let mut value: toml::Value = toml::from_str(&text).map_err(|error| {
            PluginError::InvalidPackage(format!("{} is not valid TOML: {error}", source.display()))
        })?;
        let table = value.as_table_mut().ok_or_else(|| {
            PluginError::InvalidPackage(format!("{} TOML root must be a table", source.display()))
        })?;
        let original = table
            .get("id")
            .and_then(toml::Value::as_str)
            .or_else(|| source.file_stem().and_then(|stem| stem.to_str()))
            .ok_or_else(|| PluginError::InvalidPackage("component has no usable id".to_owned()))?;
        let local = safe_namespace(original);
        table.insert(
            "id".to_owned(),
            toml::Value::String(format!("plugin-{namespace}-{local}")),
        );
        if kind == "hooks" {
            table.insert("enabled".to_owned(), toml::Value::Boolean(false));
        }
        let encoded = toml::to_string_pretty(&value).map_err(|error| {
            PluginError::InvalidPackage(format!("could not encode component: {error}"))
        })?;
        atomic_write(target, encoded.as_bytes())?;
    } else {
        fs::copy(source, target).map_err(|source_error| PluginError::Io {
            path: target.to_path_buf(),
            source: source_error,
        })?;
    }
    Ok(())
}

fn path_to_registry(path: &Path) -> Result<String, PluginError> {
    validate_relative_path(path)?;
    Ok(path.to_string_lossy().replace('\\', "/"))
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, PluginError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginError::InvalidPackage(format!(
            "{} must be a regular file",
            path.display()
        )));
    }
    if metadata.len() > limit as u64 {
        return Err(PluginError::InvalidPackage(format!(
            "{} exceeds {limit} bytes",
            path.display()
        )));
    }
    fs::read(path).map_err(|source| PluginError::Io {
        path: path.to_path_buf(),
        source,
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PluginError> {
    let parent = path
        .parent()
        .ok_or_else(|| PluginError::InvalidPackage(format!("{} has no parent", path.display())))?;
    fs::create_dir_all(parent).map_err(|source| PluginError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| PluginError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.as_file_mut().sync_all())
        .map_err(|source| PluginError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary.persist(path).map_err(|error| PluginError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Write,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Instant,
    };

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::{PluginError, PluginManager, PluginManifest, inspect_package, validate_remote_url};

    fn package(
        path: &Path,
        version: &str,
        malicious: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        package_with_id(path, "dev.example.review", version, malicious)
    }

    fn package_with_id(
        path: &Path,
        id: &str,
        version: &str,
        malicious: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file = fs::File::create(path)?;
        let mut writer = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        writer.start_file("plugin.json", options)?;
        writer.write_all(
            format!(
                r#"{{"schema_version":1,"id":"{id}","name":"Review pack","version":"{version}","description":"fixture","publisher":"test","components":{{"skills":["skills/review"],"commands":["commands/review.toml"],"hooks":["hooks/guard.toml"]}}}}"#
            )
            .as_bytes(),
        )?;
        writer.start_file("skills/review/SKILL.md", options)?;
        writer.write_all(b"---\nname: Review\ndescription: Review code\n---\nDo review")?;
        writer.start_file("commands/review.toml", options)?;
        writer.write_all(b"name='Review'\nprompt='Review this'\n")?;
        writer.start_file("hooks/guard.toml", options)?;
        writer.write_all(b"name='Guard'\nevent='pre_tool_use'\nprogram='guard'\nenabled=true\n")?;
        if malicious {
            writer.start_file("../escape.txt", options)?;
            writer.write_all(b"escape")?;
        }
        writer.finish()?;
        Ok(())
    }

    #[tokio::test]
    async fn install_enable_disable_and_remove_materializes_owned_components()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let bundle = root.path().join("review.plugin.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            root.path().join("plugins"),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(2),
        )?;
        manager.install_local(&bundle).await?;
        let snapshot = manager.snapshot();
        assert_eq!(snapshot.plugins.len(), 1);
        assert!(!snapshot.plugins[0].enabled);
        manager.set_enabled("dev.example.review", true)?;
        let hook = integration
            .path()
            .join("hooks/plugin-dev-example-review-guard.toml");
        let hook_text = fs::read_to_string(&hook)?;
        assert!(hook_text.contains("enabled = false"));
        assert!(
            integration
                .path()
                .join("skills/plugin-dev-example-review-review/SKILL.md")
                .exists()
        );
        manager.set_enabled("dev.example.review", false)?;
        assert!(!hook.exists());
        manager.remove("dev.example.review")?;
        assert!(manager.snapshot().plugins.is_empty());
        Ok(())
    }

    #[test]
    fn traversal_package_is_rejected_before_writes() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let bundle = root.path().join("bad.zip");
        package(&bundle, "1.0.0", true)?;
        let bytes = fs::read(bundle)?;
        assert!(inspect_package(bytes).is_err());
        Ok(())
    }

    #[test]
    fn manifest_round_trip_is_strict() -> Result<(), Box<dyn std::error::Error>> {
        let manifest: PluginManifest = serde_json::from_str(
            r#"{"schema_version":1,"id":"dev.example","name":"Example","version":"1.0.0","components":{}}"#,
        )?;
        assert_eq!(manifest.id, "dev.example");
        let unknown = r#"{"schema_version":1,"id":"dev.example","name":"Example","version":"1.0.0","components":{},"surprise":true}"#;
        assert!(serde_json::from_str::<PluginManifest>(unknown).is_err());
        Ok(())
    }

    #[test]
    fn registry_rejects_activation_paths_it_does_not_own() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        fs::create_dir(&plugin_root)?;
        fs::write(
            plugin_root.join(super::REGISTRY_FILE),
            serde_json::to_vec(&serde_json::json!({
                "schema_version": 1,
                "marketplaces": [],
                "installed": {
                    "dev.example.review": {
                        "manifest": {
                            "schema_version": 1,
                            "id": "dev.example.review",
                            "name": "Review",
                            "version": "1.0.0",
                            "components": {}
                        },
                        "enabled": true,
                        "source": "test",
                        "package_sha256": "00",
                        "activated_paths": ["skills"]
                    }
                }
            }))?,
        )?;

        assert!(matches!(
            PluginManager::open(
                plugin_root,
                root.path().join("integration"),
                std::time::Duration::from_secs(1),
            ),
            Err(PluginError::Registry(_))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn activation_collision_rolls_back_every_created_component()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let bundle = root.path().join("review.plugin.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            root.path().join("plugins"),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(2),
        )?;
        manager.install_local(&bundle).await?;
        let hook = integration
            .path()
            .join("hooks/plugin-dev-example-review-guard.toml");
        fs::create_dir_all(hook.parent().ok_or("hook parent")?)?;
        fs::write(&hook, "owned by user")?;

        assert!(manager.set_enabled("dev.example.review", true).is_err());
        assert!(
            !integration
                .path()
                .join("commands/plugin-dev-example-review-review.toml")
                .exists()
        );
        assert!(
            !integration
                .path()
                .join("skills/plugin-dev-example-review-review")
                .exists()
        );
        assert_eq!(fs::read_to_string(hook)?, "owned by user");
        assert!(!manager.snapshot().plugins[0].enabled);
        Ok(())
    }

    #[tokio::test]
    async fn case_only_plugin_ids_cannot_share_one_storage_namespace()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let uppercase = root.path().join("uppercase.zip");
        let lowercase = root.path().join("lowercase.zip");
        package_with_id(&uppercase, "Dev.Example.Review", "1.0.0", false)?;
        package_with_id(&lowercase, "dev.example.review", "2.0.0", false)?;
        let mut manager = PluginManager::open(
            root.path().join("plugins"),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(2),
        )?;
        manager.install_local(&uppercase).await?;

        assert!(manager.install_local(&lowercase).await.is_err());
        assert_eq!(manager.snapshot().plugins.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn install_rejects_a_symlinked_package_namespace()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let redirected = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        let bundle = root.path().join("review.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            plugin_root.clone(),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(1),
        )?;
        let namespace = plugin_root
            .join(super::PACKAGES_DIR)
            .join("dev-example-review");
        #[cfg(unix)]
        std::os::unix::fs::symlink(redirected.path(), &namespace)?;
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(&namespace)
                .arg(redirected.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            assert!(status.success(), "could not create test junction");
        }

        assert!(manager.install_local(&bundle).await.is_err());
        assert!(!redirected.path().join("1.2.3").exists());
        assert!(manager.snapshot().plugins.is_empty());
        Ok(())
    }

    #[test]
    fn every_loopback_ip_is_allowed_for_local_marketplaces() {
        assert!(validate_remote_url("http://127.0.0.2/catalog.json").is_ok());
        assert!(validate_remote_url("http://localhost./catalog.json").is_ok());
    }

    #[test]
    fn plugin_storage_root_must_not_be_a_symlink() -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let redirected = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        #[cfg(unix)]
        std::os::unix::fs::symlink(redirected.path(), &plugin_root)?;
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(&plugin_root)
                .arg(redirected.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?;
            assert!(status.success(), "could not create test junction");
        }

        assert!(
            PluginManager::open(
                plugin_root,
                root.path().join("integration"),
                std::time::Duration::from_secs(1),
            )
            .is_err()
        );
        assert!(!redirected.path().join(super::PACKAGES_DIR).exists());
        Ok(())
    }

    #[tokio::test]
    async fn chunked_download_stops_as_soon_as_the_limit_is_exceeded()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n0123456789abcdefX")
                .await?;
            stream.flush().await?;
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            Ok::<_, std::io::Error>(())
        });
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let manager = PluginManager::open(
            root.path().join("plugins"),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(5),
        )?;

        let started = Instant::now();
        let error = match manager
            .download_bounded(&format!("http://{address}/package"), 16)
            .await
        {
            Ok(_) => return Err("oversized body was accepted".into()),
            Err(error) => error,
        };
        assert!(error.to_string().contains("exceeds 16 bytes"));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        server.abort();
        Ok(())
    }

    #[tokio::test]
    async fn marketplace_redirect_is_checked_before_following_it()
    -> Result<(), Box<dyn std::error::Error>> {
        let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
        let port = listener.local_addr()?.port();
        let followed = Arc::new(AtomicBool::new(false));
        let followed_by_server = Arc::clone(&followed);
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await?;
            let mut request = [0_u8; 1024];
            let _ = first.read(&mut request).await?;
            first
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://user@127.0.0.1:{port}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await?;
            first.flush().await?;
            if let Ok(Ok((mut second, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(1), listener.accept()).await
            {
                followed_by_server.store(true, Ordering::SeqCst);
                let _ = second.read(&mut request).await?;
                second
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let manager = PluginManager::open(
            root.path().join("plugins"),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(2),
        )?;

        assert!(
            manager
                .download_bounded(&format!("http://127.0.0.1:{port}/start"), 16)
                .await
                .is_err()
        );
        server.await??;
        assert!(!followed.load(Ordering::SeqCst));
        Ok(())
    }

    #[tokio::test]
    async fn failed_registry_write_rolls_back_plugin_install()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        let bundle = root.path().join("review.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            plugin_root.clone(),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(1),
        )?;
        fs::create_dir(plugin_root.join(super::REGISTRY_FILE))?;

        assert!(manager.install_local(&bundle).await.is_err());
        assert!(manager.snapshot().plugins.is_empty());
        assert!(
            !plugin_root
                .join(super::PACKAGES_DIR)
                .join("dev-example-review")
                .join("1.2.3")
                .exists()
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_registry_write_rolls_back_plugin_enable()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        let bundle = root.path().join("review.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            plugin_root.clone(),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(1),
        )?;
        manager.install_local(&bundle).await?;
        fs::remove_file(plugin_root.join(super::REGISTRY_FILE))?;
        fs::create_dir(plugin_root.join(super::REGISTRY_FILE))?;

        assert!(manager.set_enabled("dev.example.review", true).is_err());
        assert!(!manager.snapshot().plugins[0].enabled);
        assert!(
            !integration
                .path()
                .join("skills/plugin-dev-example-review-review")
                .exists()
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_registry_write_rolls_back_plugin_disable()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        let bundle = root.path().join("review.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            plugin_root.clone(),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(1),
        )?;
        manager.install_local(&bundle).await?;
        manager.set_enabled("dev.example.review", true)?;
        fs::remove_file(plugin_root.join(super::REGISTRY_FILE))?;
        fs::create_dir(plugin_root.join(super::REGISTRY_FILE))?;

        assert!(manager.set_enabled("dev.example.review", false).is_err());
        assert!(manager.snapshot().plugins[0].enabled);
        assert!(
            integration
                .path()
                .join("skills/plugin-dev-example-review-review")
                .exists()
        );
        Ok(())
    }

    #[tokio::test]
    async fn failed_registry_write_does_not_remove_an_installed_package()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let integration = tempfile::tempdir()?;
        let plugin_root = root.path().join("plugins");
        let bundle = root.path().join("review.zip");
        package(&bundle, "1.2.3", false)?;
        let mut manager = PluginManager::open(
            plugin_root.clone(),
            integration.path().to_path_buf(),
            std::time::Duration::from_secs(1),
        )?;
        manager.install_local(&bundle).await?;
        let package = plugin_root
            .join(super::PACKAGES_DIR)
            .join("dev-example-review")
            .join("1.2.3");
        fs::remove_file(plugin_root.join(super::REGISTRY_FILE))?;
        fs::create_dir(plugin_root.join(super::REGISTRY_FILE))?;

        assert!(manager.remove("dev.example.review").is_err());
        assert_eq!(manager.snapshot().plugins.len(), 1);
        assert!(package.exists());
        Ok(())
    }
}
