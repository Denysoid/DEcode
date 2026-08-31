use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fmt,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    process,
    sync::{
        Arc, Mutex, TryLockError, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

#[cfg(windows)]
use cap_fs_ext::MetadataExt as _;
#[cfg(unix)]
use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
use cap_std::{
    ambient_authority,
    fs::{Dir, File, Metadata, OpenOptions, Permissions},
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::privacy::{PrivacyError, PrivacyShield};

const TEMP_FILE_ATTEMPTS: usize = 128;
const WRITER_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(2);
const MAX_DIRECTORY_ENTRY_NAMES: usize = 4_096;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PathViolation {
    #[error("absolute paths are forbidden")]
    Absolute,

    #[error("parent traversal components are forbidden")]
    ParentTraversal,

    #[error("platform path prefixes are forbidden")]
    PlatformPrefix,

    #[error("root path components are forbidden")]
    RootComponent,

    #[error("an empty path is not valid for this operation")]
    Empty,

    #[error("NUL bytes are forbidden in model-supplied paths")]
    NulByte,

    #[error("Windows alternate data stream paths are forbidden")]
    AlternateDataStream,

    #[error("Windows path aliases and reserved device names are forbidden")]
    WindowsPathAlias,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error(transparent)]
    Privacy(#[from] PrivacyError),

    #[error("could not open project root {path:?}: {source}")]
    OpenRoot {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "requested path {requested:?} is not allowed under project root \
         {root:?}: {violation}"
    )]
    PathEscape {
        requested: PathBuf,
        root: PathBuf,
        violation: PathViolation,
    },

    #[error(
        "symbolic links are forbidden by sandbox policy: path \
         {requested:?}, component {component:?}"
    )]
    SymlinkForbidden {
        requested: PathBuf,
        component: PathBuf,
    },

    #[error("path component {component:?} in {requested:?} is not a directory")]
    NonDirectoryComponent {
        requested: PathBuf,
        component: PathBuf,
    },

    #[error("path {requested:?} is not a regular file")]
    NotRegularFile { requested: PathBuf },

    #[error("path {requested:?} is not a directory")]
    NotDirectory { requested: PathBuf },

    #[error(
        "sandbox operation `{operation}` failed for {requested:?}: \
         {source}"
    )]
    Io {
        operation: &'static str,
        requested: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "file {requested:?} is larger than the permitted \
         {limit_bytes} bytes"
    )]
    FileTooLarge {
        requested: PathBuf,
        limit_bytes: usize,
    },

    #[error("path {requested:?} has no final file name")]
    MissingFileName { requested: PathBuf },

    #[error(
        "could not reserve a temporary file beside {requested:?} after \
         {attempts} attempts"
    )]
    TemporaryFileExhausted { requested: PathBuf, attempts: usize },

    #[error(
        "configured byte limit {limit_bytes} cannot be represented by \
         this platform"
    )]
    UnsupportedByteLimit { limit_bytes: usize },

    #[error(
        "destination {requested:?} changed while an atomic operation \
         was being prepared; the operation was aborted"
    )]
    DestinationChanged { requested: PathBuf },

    #[error(
        "file {requested:?} was modified concurrently while an atomic write \
         was being prepared; the operation was aborted"
    )]
    ConcurrentModification { requested: PathBuf },

    #[error("sandbox writer lock was poisoned for {requested:?}")]
    WriterLockPoisoned { requested: PathBuf },

    #[error(
        "path component {component:?} in {requested:?} changed while it \
         was being opened"
    )]
    ComponentChanged {
        requested: PathBuf,
        component: PathBuf,
    },

    #[error("project root {root:?} no longer names the opened workspace capability")]
    RootChanged { root: PathBuf },

    #[error("sandbox operation `{operation}` was cancelled for {requested:?}")]
    Cancelled {
        operation: &'static str,
        requested: PathBuf,
    },
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContentFingerprint {
    byte_len: u64,
    sha256: [u8; 32],
}

impl ContentFingerprint {
    fn from_bytes(bytes: &[u8]) -> Result<Self, SandboxError> {
        let byte_len =
            u64::try_from(bytes.len()).map_err(|_| SandboxError::UnsupportedByteLimit {
                limit_bytes: bytes.len(),
            })?;

        Ok(Self {
            byte_len,
            sha256: Sha256::digest(bytes).into(),
        })
    }
}

enum DestinationSnapshot {
    Missing,
    Existing {
        identity: FileIdentity,
        fingerprint: ContentFingerprint,
        permissions_fingerprint: PermissionFingerprint,
        permissions: Permissions,
    },
}

impl DestinationSnapshot {
    fn permissions(&self) -> Option<Permissions> {
        match self {
            Self::Missing => None,
            Self::Existing { permissions, .. } => Some(permissions.clone()),
        }
    }

    fn matches_state(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Missing, Self::Missing) => true,
            (
                Self::Existing {
                    identity: left_identity,
                    fingerprint: left_fingerprint,
                    permissions_fingerprint: left_permissions,
                    ..
                },
                Self::Existing {
                    identity: right_identity,
                    fingerprint: right_fingerprint,
                    permissions_fingerprint: right_permissions,
                    ..
                },
            ) => {
                left_identity == right_identity
                    && left_fingerprint == right_fingerprint
                    && left_permissions == right_permissions
            }
            _ => false,
        }
    }

    fn matches_content(&self, expected: &[u8]) -> Result<bool, SandboxError> {
        let expected_fingerprint = ContentFingerprint::from_bytes(expected)?;
        Ok(matches!(
            self,
            Self::Existing { fingerprint, .. } if *fingerprint == expected_fingerprint
        ))
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
type PermissionFingerprint = u32;

#[cfg(windows)]
type PermissionFingerprint = bool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SandboxEntryKind {
    File,
    Directory,
    Other,
}

/// Лексически нормализованный путь, привязанный к операциям SandboxRoot.
///
/// Тип не экспортируется из `tools`, не предоставляет ambient absolute path
/// и сам по себе не считается доказательством безопасности. Каждая реальная
/// операция повторно открывает компоненты через `cap_std::fs::Dir`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SandboxPath {
    relative: PathBuf,
    requested: PathBuf,
}

impl SandboxPath {
    pub(crate) fn requested_path(&self) -> &Path {
        &self.requested
    }

    pub(crate) fn relative_path(&self) -> &Path {
        &self.relative
    }
}

impl fmt::Debug for SandboxPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxPath")
            .field("relative", &self.relative)
            .field("requested", &self.requested)
            .finish()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct AtomicWriteOptions<'a> {
    create_parents: bool,
    expected_destination: ExpectedDestination<'a>,
    baseline_limit_bytes: usize,
}

#[derive(Clone, Copy)]
enum ExpectedDestination<'a> {
    Any,
    Existing(&'a [u8]),
    Missing,
}

pub(crate) struct CheckpointRestore<'a> {
    pub(crate) expected_content: Option<&'a [u8]>,
    pub(crate) expected_executable: Option<bool>,
    pub(crate) desired_content: Option<&'a [u8]>,
    pub(crate) desired_executable: Option<bool>,
    pub(crate) limit_bytes: usize,
}

impl<'a> AtomicWriteOptions<'a> {
    pub(crate) const fn capture_destination(
        create_parents: bool,
        baseline_limit_bytes: usize,
    ) -> Self {
        Self {
            create_parents,
            expected_destination: ExpectedDestination::Any,
            baseline_limit_bytes,
        }
    }

    pub(crate) const fn expect_content(
        create_parents: bool,
        expected_content: &'a [u8],
        baseline_limit_bytes: usize,
    ) -> Self {
        Self {
            create_parents,
            expected_destination: ExpectedDestination::Existing(expected_content),
            baseline_limit_bytes,
        }
    }

    pub(crate) const fn expect_missing(create_parents: bool, baseline_limit_bytes: usize) -> Self {
        Self {
            create_parents,
            expected_destination: ExpectedDestination::Missing,
            baseline_limit_bytes,
        }
    }
}

/// Корневая capability проекта.
///
/// Политика симлинков намеренно строгая: все симлинки в путях файловых
/// инструментов запрещены. Это исключает выход наружу и не допускает
/// неожиданного уничтожения внутренних симлинков при atomic rename.
#[derive(Clone)]
pub struct SandboxRoot {
    directory: Arc<Dir>,
    ambient_root: Arc<PathBuf>,
    privacy: PrivacyShield,
    writer_locks: Arc<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>>,
}

impl fmt::Debug for SandboxRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxRoot")
            .field("ambient_root", &self.ambient_root)
            .finish_non_exhaustive()
    }
}

impl SandboxRoot {
    /// Канонизирует и один раз открывает корень проекта как capability.
    pub fn open(project_root: &Path) -> Result<Self, SandboxError> {
        let canonical_root =
            std::fs::canonicalize(project_root).map_err(|source| SandboxError::OpenRoot {
                path: project_root.to_path_buf(),
                source,
            })?;

        let privacy = PrivacyShield::load_project_only(&canonical_root)?;
        Self::open_canonical(canonical_root, privacy)
    }

    pub(crate) fn open_with_privacy(
        project_root: &Path,
        privacy: PrivacyShield,
    ) -> Result<Self, SandboxError> {
        let canonical_root =
            std::fs::canonicalize(project_root).map_err(|source| SandboxError::OpenRoot {
                path: project_root.to_path_buf(),
                source,
            })?;
        Self::open_canonical(canonical_root, privacy)
    }

    fn open_canonical(
        canonical_root: PathBuf,
        privacy: PrivacyShield,
    ) -> Result<Self, SandboxError> {
        let directory =
            Dir::open_ambient_dir(&canonical_root, ambient_authority()).map_err(|source| {
                SandboxError::OpenRoot {
                    path: canonical_root.clone(),
                    source,
                }
            })?;

        Ok(Self {
            directory: Arc::new(directory),
            ambient_root: Arc::new(canonical_root),
            privacy,
            writer_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub(crate) fn model_file_path(&self, requested: &str) -> Result<SandboxPath, SandboxError> {
        let path = self.resolve_model_path(requested, false)?;
        self.privacy.check_relative(path.relative_path(), false)?;
        Ok(path)
    }

    pub(crate) fn model_directory_path(
        &self,
        requested: &str,
    ) -> Result<SandboxPath, SandboxError> {
        let path = self.resolve_model_path(requested, true)?;
        self.privacy.check_relative(path.relative_path(), true)?;
        Ok(path)
    }

    pub(crate) fn resolve_candidate(
        &self,
        candidate: &Path,
        allow_root: bool,
    ) -> Result<SandboxPath, SandboxError> {
        self.resolve_path(candidate, allow_root)
    }

    pub(crate) fn check_privacy(
        &self,
        candidate: &SandboxPath,
        is_directory: bool,
    ) -> Result<(), SandboxError> {
        self.privacy
            .check_relative(candidate.relative_path(), is_directory)
            .map_err(SandboxError::from)
    }

    fn resolve_model_path(
        &self,
        requested: &str,
        allow_root: bool,
    ) -> Result<SandboxPath, SandboxError> {
        if requested.as_bytes().contains(&0) {
            return Err(self.path_escape(PathBuf::from(requested), PathViolation::NulByte));
        }

        self.resolve_path(Path::new(requested), allow_root)
    }

    fn resolve_path(
        &self,
        requested: &Path,
        allow_root: bool,
    ) -> Result<SandboxPath, SandboxError> {
        if requested.is_absolute() {
            return Err(self.path_escape(requested.to_path_buf(), PathViolation::Absolute));
        }

        let mut relative = PathBuf::new();

        for component in requested.components() {
            match component {
                Component::Normal(value) => {
                    #[cfg(windows)]
                    {
                        if value.as_encoded_bytes().contains(&b':') {
                            return Err(self.path_escape(
                                requested.to_path_buf(),
                                PathViolation::AlternateDataStream,
                            ));
                        }
                        if is_unsafe_windows_component(value) {
                            return Err(self.path_escape(
                                requested.to_path_buf(),
                                PathViolation::WindowsPathAlias,
                            ));
                        }
                    }
                    relative.push(value);
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    return Err(
                        self.path_escape(requested.to_path_buf(), PathViolation::ParentTraversal)
                    );
                }
                Component::Prefix(_) => {
                    return Err(
                        self.path_escape(requested.to_path_buf(), PathViolation::PlatformPrefix)
                    );
                }
                Component::RootDir => {
                    return Err(
                        self.path_escape(requested.to_path_buf(), PathViolation::RootComponent)
                    );
                }
            }
        }

        if relative.as_os_str().is_empty() && !allow_root {
            return Err(self.path_escape(requested.to_path_buf(), PathViolation::Empty));
        }

        Ok(SandboxPath {
            relative,
            requested: requested.to_path_buf(),
        })
    }

    fn path_escape(&self, requested: PathBuf, violation: PathViolation) -> SandboxError {
        SandboxError::PathEscape {
            requested,
            root: self.ambient_root.as_ref().clone(),
            violation,
        }
    }

    fn clone_root(&self, requested: &Path) -> Result<Dir, SandboxError> {
        self.directory
            .as_ref()
            .try_clone()
            .map_err(|source| SandboxError::Io {
                operation: "clone project root capability",
                requested: requested.to_path_buf(),
                source,
            })
    }

    fn open_directory_components(
        &self,
        relative: &Path,
        requested: &Path,
        create_missing: bool,
    ) -> Result<Dir, SandboxError> {
        let mut current = self.clone_root(requested)?;
        let mut traversed = PathBuf::new();

        for component in relative.components() {
            let Component::Normal(name) = component else {
                continue;
            };

            traversed.push(name);
            let component_path = Path::new(name);

            let metadata = match current.symlink_metadata(component_path) {
                Ok(value) => value,
                Err(source) if source.kind() == std::io::ErrorKind::NotFound && create_missing => {
                    match current.create_dir(component_path) {
                        Ok(()) => {}
                        Err(create_error)
                            if create_error.kind() == std::io::ErrorKind::AlreadyExists =>
                        {
                            // Между проверкой и create другой процесс мог
                            // создать компонент. Ниже он проверяется заново.
                        }
                        Err(create_error) => {
                            return Err(SandboxError::Io {
                                operation: "create directory component",
                                requested: requested.to_path_buf(),
                                source: create_error,
                            });
                        }
                    }

                    current
                        .symlink_metadata(component_path)
                        .map_err(|metadata_error| SandboxError::Io {
                            operation: "inspect created directory component",
                            requested: requested.to_path_buf(),
                            source: metadata_error,
                        })?
                }
                Err(source) => {
                    return Err(SandboxError::Io {
                        operation: "inspect directory component",
                        requested: requested.to_path_buf(),
                        source,
                    });
                }
            };

            if metadata.file_type().is_symlink() {
                return Err(SandboxError::SymlinkForbidden {
                    requested: requested.to_path_buf(),
                    component: traversed,
                });
            }

            if !metadata.is_dir() {
                return Err(SandboxError::NonDirectoryComponent {
                    requested: requested.to_path_buf(),
                    component: traversed,
                });
            }

            let opened = current
                .open_dir(component_path)
                .map_err(|source| SandboxError::Io {
                    operation: "open directory component",
                    requested: requested.to_path_buf(),
                    source,
                })?;

            let opened_metadata = opened.dir_metadata().map_err(|source| SandboxError::Io {
                operation: "inspect opened directory component",
                requested: requested.to_path_buf(),
                source,
            })?;

            if !opened_metadata.is_dir() || !same_metadata_identity(&metadata, &opened_metadata) {
                return Err(SandboxError::ComponentChanged {
                    requested: requested.to_path_buf(),
                    component: traversed,
                });
            }

            current = opened;
        }

        Ok(current)
    }

    fn open_parent<'a>(
        &self,
        path: &'a SandboxPath,
        create_missing: bool,
    ) -> Result<(Dir, &'a OsStr), SandboxError> {
        let file_name = path
            .relative
            .file_name()
            .ok_or_else(|| SandboxError::MissingFileName {
                requested: path.requested.clone(),
            })?;

        let parent = match path.relative.parent() {
            Some(value) => value,
            None => Path::new(""),
        };

        let directory = self.open_directory_components(parent, &path.requested, create_missing)?;

        Ok((directory, file_name))
    }

    fn optional_metadata(
        &self,
        directory: &Dir,
        file_name: &OsStr,
        path: &SandboxPath,
    ) -> Result<Option<Metadata>, SandboxError> {
        match directory.symlink_metadata(Path::new(file_name)) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(SandboxError::Io {
                operation: "inspect destination",
                requested: path.requested.clone(),
                source,
            }),
        }
    }

    fn validate_regular_metadata(
        &self,
        metadata: &Metadata,
        path: &SandboxPath,
    ) -> Result<(), SandboxError> {
        if metadata.file_type().is_symlink() {
            return Err(SandboxError::SymlinkForbidden {
                requested: path.requested.clone(),
                component: path.relative.clone(),
            });
        }

        if !metadata.is_file() {
            return Err(SandboxError::NotRegularFile {
                requested: path.requested.clone(),
            });
        }

        Ok(())
    }

    pub(crate) fn ensure_directory(&self, path: &SandboxPath) -> Result<(), SandboxError> {
        if path.relative.as_os_str().is_empty() {
            let root = self.clone_root(&path.requested)?;
            drop(root);
            return Ok(());
        }

        let directory = self.open_directory_components(&path.relative, &path.requested, false)?;

        drop(directory);
        Ok(())
    }

    pub(crate) fn verify_ambient_root_identity(&self) -> Result<(), SandboxError> {
        let path_metadata =
            std::fs::symlink_metadata(self.ambient_root.as_path()).map_err(|source| {
                SandboxError::Io {
                    operation: "inspect ambient project root",
                    requested: self.ambient_root.as_ref().clone(),
                    source,
                }
            })?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_dir() {
            return Err(SandboxError::RootChanged {
                root: self.ambient_root.as_ref().clone(),
            });
        }

        let reopened = Dir::open_ambient_dir(self.ambient_root.as_path(), ambient_authority())
            .map_err(|source| SandboxError::Io {
                operation: "reopen ambient project root",
                requested: self.ambient_root.as_ref().clone(),
                source,
            })?;
        let held_metadata = self
            .directory
            .dir_metadata()
            .map_err(|source| SandboxError::Io {
                operation: "inspect held project root capability",
                requested: self.ambient_root.as_ref().clone(),
                source,
            })?;
        let reopened_metadata = reopened.dir_metadata().map_err(|source| SandboxError::Io {
            operation: "inspect reopened project root",
            requested: self.ambient_root.as_ref().clone(),
            source,
        })?;

        if !same_metadata_identity(&held_metadata, &reopened_metadata) {
            return Err(SandboxError::RootChanged {
                root: self.ambient_root.as_ref().clone(),
            });
        }
        Ok(())
    }

    /// Enumerate one already-validated directory through its capability.
    ///
    /// Per-entry I/O failures are counted rather than turning a large listing
    /// into an all-or-nothing operation. The directory itself failing to open
    /// remains a hard error.
    pub(crate) fn directory_entry_names(
        &self,
        path: &SandboxPath,
    ) -> Result<(Vec<OsString>, usize, bool), SandboxError> {
        let directory = self.open_directory_components(&path.relative, &path.requested, false)?;
        let entries = directory.entries().map_err(|source| SandboxError::Io {
            operation: "enumerate directory",
            requested: path.requested.clone(),
            source,
        })?;

        let mut names = Vec::new();
        let mut errors = 0usize;
        let mut limit_reached = false;

        for entry in entries {
            match entry {
                Ok(entry) => {
                    if names.len() >= MAX_DIRECTORY_ENTRY_NAMES {
                        limit_reached = true;
                        break;
                    }
                    let name = entry.file_name();
                    if name.as_os_str() != OsStr::new(".") && name.as_os_str() != OsStr::new("..") {
                        names.push(name);
                    }
                }
                Err(_) => errors = errors.saturating_add(1),
            }
        }

        names.sort();
        Ok((names, errors, limit_reached))
    }

    pub(crate) fn with_target_write_lock_cancellable<T, E, F>(
        &self,
        path: &SandboxPath,
        cancel: &CancellationToken,
        operation_name: &'static str,
        operation: F,
    ) -> Result<T, E>
    where
        E: From<SandboxError>,
        F: FnOnce() -> Result<T, E>,
    {
        let lock = self.target_write_lock(path).map_err(E::from)?;
        let _guard = loop {
            check_cancellation(Some((cancel, operation_name)), path).map_err(E::from)?;
            match lock.try_lock() {
                Ok(guard) => break guard,
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(WRITER_LOCK_POLL_INTERVAL);
                }
                Err(TryLockError::Poisoned(_)) => {
                    return Err(E::from(SandboxError::WriterLockPoisoned {
                        requested: path.requested.clone(),
                    }));
                }
            }
        };

        operation()
    }

    fn target_write_lock(&self, path: &SandboxPath) -> Result<Arc<Mutex<()>>, SandboxError> {
        let mut locks = self
            .writer_locks
            .lock()
            .map_err(|_| SandboxError::WriterLockPoisoned {
                requested: path.requested.clone(),
            })?;

        locks.retain(|_, lock| lock.strong_count() > 0);

        let key = writer_lock_key(&path.relative);
        if let Some(existing) = locks.get(&key).and_then(Weak::upgrade) {
            return Ok(existing);
        }

        let lock = Arc::new(Mutex::new(()));
        locks.insert(key, Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(crate) fn entry_kind(&self, path: &SandboxPath) -> Result<SandboxEntryKind, SandboxError> {
        if path.relative.as_os_str().is_empty() {
            return Ok(SandboxEntryKind::Directory);
        }

        let (parent, file_name) = self.open_parent(path, false)?;
        let metadata = self
            .optional_metadata(&parent, file_name, path)?
            .ok_or_else(|| SandboxError::Io {
                operation: "inspect entry",
                requested: path.requested.clone(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "entry no longer exists"),
            })?;

        if metadata.file_type().is_symlink() {
            return Err(SandboxError::SymlinkForbidden {
                requested: path.requested.clone(),
                component: path.relative.clone(),
            });
        }

        if metadata.is_file() {
            Ok(SandboxEntryKind::File)
        } else if metadata.is_dir() {
            Ok(SandboxEntryKind::Directory)
        } else {
            Ok(SandboxEntryKind::Other)
        }
    }

    pub(crate) fn read_regular_file_limited(
        &self,
        path: &SandboxPath,
        limit_bytes: usize,
    ) -> Result<Vec<u8>, SandboxError> {
        let limit_u64 = u64::try_from(limit_bytes)
            .map_err(|_| SandboxError::UnsupportedByteLimit { limit_bytes })?;

        let (parent, file_name) = self.open_parent(path, false)?;

        let metadata = self
            .optional_metadata(&parent, file_name, path)?
            .ok_or_else(|| SandboxError::Io {
                operation: "open regular file",
                requested: path.requested.clone(),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "file does not exist"),
            })?;

        self.validate_regular_metadata(&metadata, path)?;

        if metadata.len() > limit_u64 {
            return Err(SandboxError::FileTooLarge {
                requested: path.requested.clone(),
                limit_bytes,
            });
        }

        let file = parent
            .open(Path::new(file_name))
            .map_err(|source| SandboxError::Io {
                operation: "open regular file",
                requested: path.requested.clone(),
                source,
            })?;

        let opened_metadata = file.metadata().map_err(|source| SandboxError::Io {
            operation: "inspect opened file",
            requested: path.requested.clone(),
            source,
        })?;

        if !opened_metadata.is_file() || !same_metadata_identity(&metadata, &opened_metadata) {
            return Err(SandboxError::DestinationChanged {
                requested: path.requested.clone(),
            });
        }

        let read_limit = limit_u64.saturating_add(1);
        let mut limited_reader = file.take(read_limit);
        let mut bytes = Vec::new();

        limited_reader
            .read_to_end(&mut bytes)
            .map_err(|source| SandboxError::Io {
                operation: "read regular file",
                requested: path.requested.clone(),
                source,
            })?;

        if bytes.len() > limit_bytes {
            return Err(SandboxError::FileTooLarge {
                requested: path.requested.clone(),
                limit_bytes,
            });
        }

        Ok(bytes)
    }

    /// Restores a checkpoint only while the current content and mode still
    /// match. A missing desired value removes a file created after the snapshot.
    pub(crate) fn checkpoint_compare_and_restore(
        &self,
        requested: &Path,
        restore: CheckpointRestore<'_>,
        cancel: &CancellationToken,
    ) -> Result<(), SandboxError> {
        let path = self.resolve_candidate(requested, false)?;
        self.with_target_write_lock_cancellable(&path, cancel, "restore checkpoint file", || {
            let (parent, file_name) = self.open_parent(&path, restore.desired_content.is_some())?;
            let baseline =
                self.capture_destination_snapshot(&parent, file_name, &path, restore.limit_bytes)?;
            let matches_expected = match restore.expected_content {
                Some(expected) => {
                    baseline.matches_content(expected)?
                        && checkpoint_executable_matches(&baseline, restore.expected_executable)
                }
                None => matches!(baseline, DestinationSnapshot::Missing),
            };
            if !matches_expected {
                return Err(self.concurrent_modification(&path));
            }

            match restore.desired_content {
                Some(bytes) => {
                    let options = restore.expected_content.map_or_else(
                        || AtomicWriteOptions::capture_destination(true, restore.limit_bytes),
                        |expected| {
                            AtomicWriteOptions::expect_content(true, expected, restore.limit_bytes)
                        },
                    );
                    self.atomic_write_under_lock(
                        &path,
                        bytes,
                        options,
                        Some((cancel, "restore checkpoint file")),
                    )?;
                    set_checkpoint_executable(&parent, file_name, &path, restore.desired_executable)
                }
                None => {
                    check_cancellation(Some((cancel, "restore checkpoint file")), &path)?;
                    self.verify_ambient_root_identity()?;
                    self.verify_destination_snapshot(
                        &parent,
                        file_name,
                        &path,
                        &baseline,
                        restore.limit_bytes,
                    )?;
                    if matches!(baseline, DestinationSnapshot::Missing) {
                        return Ok(());
                    }
                    parent.remove_file(Path::new(file_name)).map_err(|source| {
                        SandboxError::Io {
                            operation: "remove checkpoint-created file",
                            requested: path.requested.clone(),
                            source,
                        }
                    })?;
                    sync_directory_best_effort(&parent, &path);
                    Ok(())
                }
            }
        })
    }

    pub(crate) fn atomic_write_cancellable(
        &self,
        path: &SandboxPath,
        bytes: &[u8],
        options: AtomicWriteOptions<'_>,
        cancel: &CancellationToken,
        operation: &'static str,
    ) -> Result<(), SandboxError> {
        self.with_target_write_lock_cancellable(path, cancel, operation, || {
            self.atomic_write_under_lock(path, bytes, options, Some((cancel, operation)))
        })
    }

    pub(crate) fn atomic_write_under_lock(
        &self,
        path: &SandboxPath,
        bytes: &[u8],
        options: AtomicWriteOptions<'_>,
        cancellation: Option<(&CancellationToken, &'static str)>,
    ) -> Result<(), SandboxError> {
        self.atomic_write_under_lock_with_hook(path, bytes, options, cancellation, || Ok(()))
    }

    fn atomic_write_under_lock_with_hook<F>(
        &self,
        path: &SandboxPath,
        bytes: &[u8],
        options: AtomicWriteOptions<'_>,
        cancellation: Option<(&CancellationToken, &'static str)>,
        before_revalidation: F,
    ) -> Result<(), SandboxError>
    where
        F: FnOnce() -> Result<(), SandboxError>,
    {
        check_cancellation(cancellation, path)?;

        let (parent, file_name) = self.open_parent(path, options.create_parents)?;
        let baseline = self.capture_destination_snapshot(
            &parent,
            file_name,
            path,
            options.baseline_limit_bytes,
        )?;

        let expectation_matches = match options.expected_destination {
            ExpectedDestination::Any => true,
            ExpectedDestination::Existing(expected) => baseline.matches_content(expected)?,
            ExpectedDestination::Missing => matches!(baseline, DestinationSnapshot::Missing),
        };
        if !expectation_matches {
            return Err(self.concurrent_modification(path));
        }

        let existing_permissions = baseline.permissions();

        let (mut temporary_file, temporary_name) = create_temporary_file(&parent, path)?;

        let mut cleanup = TemporaryPathGuard::new(&parent, temporary_name.clone());

        temporary_file
            .write_all(bytes)
            .map_err(|source| SandboxError::Io {
                operation: "write temporary file",
                requested: path.requested.clone(),
                source,
            })?;

        if let Some(permissions) = existing_permissions {
            set_temporary_permissions(&temporary_file, permissions, path)?;
        }

        temporary_file.flush().map_err(|source| SandboxError::Io {
            operation: "flush temporary file",
            requested: path.requested.clone(),
            source,
        })?;

        temporary_file
            .sync_all()
            .map_err(|source| SandboxError::Io {
                operation: "synchronize temporary file",
                requested: path.requested.clone(),
                source,
            })?;

        drop(temporary_file);

        check_cancellation(cancellation, path)?;
        before_revalidation()?;
        self.verify_ambient_root_identity()?;
        self.verify_destination_snapshot(
            &parent,
            file_name,
            path,
            &baseline,
            options.baseline_limit_bytes,
        )?;

        // This is the commit boundary. Cancellation observed before here
        // leaves the destination unchanged. Once rename starts, the operation
        // reports the committed result even if the token is cancelled
        // concurrently.
        check_cancellation(cancellation, path)?;

        parent
            .rename(&temporary_name, &parent, Path::new(file_name))
            .map_err(|source| SandboxError::Io {
                operation: "atomically rename temporary file",
                requested: path.requested.clone(),
                source,
            })?;

        cleanup.disarm();
        sync_directory_best_effort(&parent, path);
        Ok(())
    }

    fn capture_destination_snapshot(
        &self,
        parent: &Dir,
        file_name: &OsStr,
        path: &SandboxPath,
        limit_bytes: usize,
    ) -> Result<DestinationSnapshot, SandboxError> {
        let Some(metadata) = self.optional_metadata(parent, file_name, path)? else {
            return Ok(DestinationSnapshot::Missing);
        };

        self.validate_regular_metadata(&metadata, path)?;

        let limit_u64 = u64::try_from(limit_bytes)
            .map_err(|_| SandboxError::UnsupportedByteLimit { limit_bytes })?;

        if metadata.len() > limit_u64 {
            return Err(SandboxError::FileTooLarge {
                requested: path.requested.clone(),
                limit_bytes,
            });
        }

        let file = parent
            .open(Path::new(file_name))
            .map_err(|source| SandboxError::Io {
                operation: "open destination for content fingerprint",
                requested: path.requested.clone(),
                source,
            })?;

        let opened_metadata = file.metadata().map_err(|source| SandboxError::Io {
            operation: "inspect destination for content fingerprint",
            requested: path.requested.clone(),
            source,
        })?;

        let path_identity =
            metadata_identity(&metadata).ok_or_else(|| self.concurrent_modification(path))?;
        let opened_identity = metadata_identity(&opened_metadata)
            .ok_or_else(|| self.concurrent_modification(path))?;

        if !opened_metadata.is_file() || path_identity != opened_identity {
            return Err(self.concurrent_modification(path));
        }

        let mut reader = file.take(limit_u64.saturating_add(1));
        let mut bytes = Vec::new();

        reader
            .read_to_end(&mut bytes)
            .map_err(|source| SandboxError::Io {
                operation: "fingerprint destination content",
                requested: path.requested.clone(),
                source,
            })?;

        if bytes.len() > limit_bytes {
            return Err(SandboxError::FileTooLarge {
                requested: path.requested.clone(),
                limit_bytes,
            });
        }

        let Some(final_metadata) = self.optional_metadata(parent, file_name, path)? else {
            return Err(self.concurrent_modification(path));
        };
        self.validate_regular_metadata(&final_metadata, path)?;

        let final_identity =
            metadata_identity(&final_metadata).ok_or_else(|| self.concurrent_modification(path))?;
        let actual_len = u64::try_from(bytes.len())
            .map_err(|_| SandboxError::UnsupportedByteLimit { limit_bytes })?;

        if final_identity != opened_identity
            || metadata.len() != actual_len
            || opened_metadata.len() != actual_len
            || final_metadata.len() != actual_len
        {
            return Err(self.concurrent_modification(path));
        }

        Ok(DestinationSnapshot::Existing {
            identity: opened_identity,
            fingerprint: ContentFingerprint::from_bytes(&bytes)?,
            permissions_fingerprint: permission_fingerprint(&final_metadata),
            permissions: final_metadata.permissions(),
        })
    }

    fn verify_destination_snapshot(
        &self,
        parent: &Dir,
        file_name: &OsStr,
        path: &SandboxPath,
        baseline: &DestinationSnapshot,
        limit_bytes: usize,
    ) -> Result<(), SandboxError> {
        let current = match self.capture_destination_snapshot(parent, file_name, path, limit_bytes)
        {
            Ok(snapshot) => snapshot,
            Err(SandboxError::FileTooLarge { .. }) => {
                return Err(self.concurrent_modification(path));
            }
            Err(error) => return Err(error),
        };

        if !baseline.matches_state(&current) {
            return Err(self.concurrent_modification(path));
        }

        Ok(())
    }

    fn concurrent_modification(&self, path: &SandboxPath) -> SandboxError {
        SandboxError::ConcurrentModification {
            requested: path.requested.clone(),
        }
    }

    pub(crate) fn ambient_root_path(&self) -> &Path {
        self.ambient_root.as_path()
    }
}

#[cfg(unix)]
fn checkpoint_executable_matches(snapshot: &DestinationSnapshot, expected: Option<bool>) -> bool {
    let Some(expected) = expected else {
        return true;
    };
    match snapshot {
        DestinationSnapshot::Existing { permissions, .. } => {
            (permissions.mode() & 0o111 != 0) == expected
        }
        DestinationSnapshot::Missing => false,
    }
}

#[cfg(windows)]
fn checkpoint_executable_matches(_snapshot: &DestinationSnapshot, _expected: Option<bool>) -> bool {
    true
}

#[cfg(unix)]
fn set_checkpoint_executable(
    parent: &Dir,
    file_name: &OsStr,
    path: &SandboxPath,
    desired: Option<bool>,
) -> Result<(), SandboxError> {
    let Some(desired) = desired else {
        return Ok(());
    };
    let file = parent
        .open(Path::new(file_name))
        .map_err(|source| SandboxError::Io {
            operation: "open restored file permissions",
            requested: path.requested.clone(),
            source,
        })?;
    let metadata = file.metadata().map_err(|source| SandboxError::Io {
        operation: "inspect restored file permissions",
        requested: path.requested.clone(),
        source,
    })?;
    let mut permissions = metadata.permissions();
    let mode = permissions.mode();
    permissions.set_mode(if desired { mode | 0o111 } else { mode & !0o111 });
    file.set_permissions(permissions)
        .map_err(|source| SandboxError::Io {
            operation: "restore executable permission",
            requested: path.requested.clone(),
            source,
        })
}

#[cfg(windows)]
fn set_checkpoint_executable(
    _parent: &Dir,
    _file_name: &OsStr,
    _path: &SandboxPath,
    _desired: Option<bool>,
) -> Result<(), SandboxError> {
    Ok(())
}

fn set_temporary_permissions(
    temporary_file: &File,
    permissions: Permissions,
    target: &SandboxPath,
) -> Result<(), SandboxError> {
    temporary_file
        .set_permissions(permissions)
        .map_err(|source| SandboxError::Io {
            operation: "preserve destination permissions",
            requested: target.requested.clone(),
            source,
        })
}

fn check_cancellation(
    cancellation: Option<(&CancellationToken, &'static str)>,
    path: &SandboxPath,
) -> Result<(), SandboxError> {
    if let Some((cancel, operation)) = cancellation
        && cancel.is_cancelled()
    {
        return Err(SandboxError::Cancelled {
            operation,
            requested: path.requested.clone(),
        });
    }

    Ok(())
}

fn sync_directory_best_effort(directory: &Dir, target: &SandboxPath) {
    let result = directory
        .try_clone()
        .and_then(|clone| clone.into_std_file().sync_all());

    if let Err(source) = result {
        // Rename has already committed. Returning an ordinary error here would
        // invite a retry while falsely implying that no mutation occurred.
        tracing::warn!(
            requested = ?target.requested,
            error = %source,
            "Atomic write committed, but the parent directory could not be synchronized"
        );
    }
}

#[cfg(unix)]
fn metadata_identity(metadata: &Metadata) -> Option<FileIdentity> {
    Some(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn metadata_identity(metadata: &Metadata) -> Option<FileIdentity> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }))
    .ok()
}

fn same_metadata_identity(left: &Metadata, right: &Metadata) -> bool {
    match (metadata_identity(left), metadata_identity(right)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

#[cfg(windows)]
fn is_unsafe_windows_component(component: &OsStr) -> bool {
    let rendered = component.to_string_lossy();
    if rendered.ends_with([' ', '.']) {
        return true;
    }

    let stem = rendered.split('.').next().unwrap_or_default();
    let uppercase = stem.to_ascii_uppercase();
    matches!(
        uppercase.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || uppercase
        .strip_prefix("COM")
        .or_else(|| uppercase.strip_prefix("LPT"))
        .is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

#[cfg(unix)]
fn permission_fingerprint(metadata: &Metadata) -> PermissionFingerprint {
    metadata.permissions().mode()
}

#[cfg(windows)]
fn permission_fingerprint(metadata: &Metadata) -> PermissionFingerprint {
    metadata.permissions().readonly()
}

#[cfg(unix)]
fn writer_lock_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(windows)]
fn writer_lock_key(path: &Path) -> PathBuf {
    // Windows workspaces are normally case-insensitive. Over-locking a
    // case-sensitive directory is safe; under-locking aliases is not.
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

fn create_temporary_file(
    parent: &Dir,
    target: &SandboxPath,
) -> Result<(File, PathBuf), SandboxError> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = PathBuf::from(format!(".decode.tmp.{}.{}", process::id(), counter));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);

        match parent.open_with(&name, &options) {
            Ok(file) => return Ok((file, name)),
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                continue;
            }
            Err(source) => {
                return Err(SandboxError::Io {
                    operation: "create temporary file",
                    requested: target.requested.clone(),
                    source,
                });
            }
        }
    }

    Err(SandboxError::TemporaryFileExhausted {
        requested: target.requested.clone(),
        attempts: TEMP_FILE_ATTEMPTS,
    })
}

struct TemporaryPathGuard<'a> {
    directory: &'a Dir,
    path: PathBuf,
    armed: bool,
}

impl<'a> TemporaryPathGuard<'a> {
    const fn new(directory: &'a Dir, path: PathBuf) -> Self {
        Self {
            directory,
            path,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryPathGuard<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Err(source) = self.directory.remove_file(&self.path)
            && source.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                temporary_path = ?self.path,
                error = %source,
                "Could not remove abandoned sandbox temporary file"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    use super::{AtomicWriteOptions, CheckpointRestore, SandboxError, SandboxRoot};

    #[test]
    fn cancellation_interrupts_writer_lock_wait() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;
        let lock = sandbox.target_write_lock(&path)?;
        let _held = lock
            .lock()
            .map_err(|_| std::io::Error::other("test lock was poisoned"))?;
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            worker_cancel.cancel();
        });

        let started = Instant::now();
        let result: Result<(), SandboxError> =
            sandbox.with_target_write_lock_cancellable(&path, &cancel, "test_write", || Ok(()));
        canceller
            .join()
            .map_err(|_| std::io::Error::other("canceller thread panicked"))?;

        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn cancellation_before_atomic_commit_preserves_destination()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        std::fs::write(root.path().join("file.txt"), "old")?;
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = sandbox.atomic_write_cancellable(
            &path,
            b"new",
            AtomicWriteOptions::capture_destination(false, 1024),
            &cancel,
            "test_write",
        );

        assert!(matches!(result, Err(SandboxError::Cancelled { .. })));
        assert_eq!(std::fs::read(root.path().join("file.txt"))?, b"old");
        Ok(())
    }

    #[test]
    fn stale_expected_content_preserves_destination_and_cleans_temporary_file()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        std::fs::write(root.path().join("file.txt"), "current")?;
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;

        let result = sandbox.atomic_write_cancellable(
            &path,
            b"replacement",
            AtomicWriteOptions::expect_content(false, b"stale", 1024),
            &CancellationToken::new(),
            "test_cas",
        );

        assert!(matches!(
            result,
            Err(SandboxError::ConcurrentModification { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("file.txt"))?, b"current");
        assert_eq!(
            std::fs::read_dir(root.path())?
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".decode.tmp."))
                .count(),
            0
        );
        Ok(())
    }

    #[test]
    fn expected_missing_refuses_to_overwrite_a_newly_created_destination()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("new.txt")?;
        std::fs::write(root.path().join("new.txt"), "manual")?;

        let result = sandbox.atomic_write_cancellable(
            &path,
            b"agent",
            AtomicWriteOptions::expect_missing(true, 1024),
            &CancellationToken::new(),
            "reviewed create",
        );

        assert!(matches!(
            result,
            Err(SandboxError::ConcurrentModification { .. })
        ));
        assert_eq!(std::fs::read(root.path().join("new.txt"))?, b"manual");
        Ok(())
    }

    #[test]
    fn restoring_missing_to_missing_is_a_successful_no_op() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = TempDir::new()?;
        let sandbox = SandboxRoot::open(root.path())?;
        let destination = root.path().join("absent.txt");

        sandbox.checkpoint_compare_and_restore(
            std::path::Path::new("absent.txt"),
            CheckpointRestore {
                expected_content: None,
                expected_executable: None,
                desired_content: None,
                desired_executable: None,
                limit_bytes: 1024,
            },
            &CancellationToken::new(),
        )?;

        assert!(!destination.exists());
        Ok(())
    }

    #[test]
    fn in_place_modification_before_rename_aborts_write_and_cleans_temporary_file()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write as _;

        let root = TempDir::new()?;
        let destination = root.path().join("file.txt");
        std::fs::write(&destination, "baseline")?;
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;
        let cancel = CancellationToken::new();

        let result: Result<(), SandboxError> =
            sandbox.with_target_write_lock_cancellable(&path, &cancel, "test_write", || {
                sandbox.atomic_write_under_lock_with_hook(
                    &path,
                    b"replacement",
                    AtomicWriteOptions::capture_destination(false, 1024),
                    Some((&cancel, "test_write")),
                    || {
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&destination)
                            .map_err(|source| SandboxError::Io {
                            operation: "test in-place destination modification",
                            requested: path.requested.clone(),
                            source,
                        })?;
                        file.write_all(b"tampered")
                            .map_err(|source| SandboxError::Io {
                                operation: "test in-place destination modification",
                                requested: path.requested.clone(),
                                source,
                            })?;
                        file.sync_all().map_err(|source| SandboxError::Io {
                            operation: "test in-place destination modification",
                            requested: path.requested.clone(),
                            source,
                        })
                    },
                )
            });

        assert!(matches!(
            result,
            Err(SandboxError::ConcurrentModification { .. })
        ));
        assert_eq!(std::fs::read(&destination)?, b"tampered");
        assert_eq!(temporary_file_count(root.path())?, 0);
        Ok(())
    }

    #[test]
    fn destination_created_during_new_file_write_is_not_overwritten()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let destination = root.path().join("file.txt");
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;
        let cancel = CancellationToken::new();

        let result: Result<(), SandboxError> =
            sandbox.with_target_write_lock_cancellable(&path, &cancel, "test_write", || {
                sandbox.atomic_write_under_lock_with_hook(
                    &path,
                    b"replacement",
                    AtomicWriteOptions::capture_destination(false, 1024),
                    Some((&cancel, "test_write")),
                    || {
                        std::fs::write(&destination, "external").map_err(|source| {
                            SandboxError::Io {
                                operation: "test concurrent destination creation",
                                requested: path.requested.clone(),
                                source,
                            }
                        })
                    },
                )
            });

        assert!(matches!(
            result,
            Err(SandboxError::ConcurrentModification { .. })
        ));
        assert_eq!(std::fs::read(&destination)?, b"external");
        assert_eq!(temporary_file_count(root.path())?, 0);
        Ok(())
    }

    #[test]
    fn permission_change_before_rename_aborts_the_write() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = TempDir::new()?;
        let destination = root.path().join("file.txt");
        std::fs::write(&destination, "baseline")?;
        let original_permissions = std::fs::metadata(&destination)?.permissions();
        let sandbox = SandboxRoot::open(root.path())?;
        let path = sandbox.model_file_path("file.txt")?;
        let cancel = CancellationToken::new();

        let result: Result<(), SandboxError> =
            sandbox.with_target_write_lock_cancellable(&path, &cancel, "test_write", || {
                sandbox.atomic_write_under_lock_with_hook(
                    &path,
                    b"replacement",
                    AtomicWriteOptions::capture_destination(false, 1024),
                    Some((&cancel, "test_write")),
                    || {
                        let mut permissions = std::fs::metadata(&destination)
                            .map_err(|source| SandboxError::Io {
                                operation: "test inspect destination permissions",
                                requested: path.requested.clone(),
                                source,
                            })?
                            .permissions();
                        permissions.set_readonly(true);
                        std::fs::set_permissions(&destination, permissions).map_err(|source| {
                            SandboxError::Io {
                                operation: "test concurrent permission change",
                                requested: path.requested.clone(),
                                source,
                            }
                        })
                    },
                )
            });
        let changed_permissions = std::fs::metadata(&destination)?.permissions();
        std::fs::set_permissions(&destination, original_permissions)?;

        assert!(matches!(
            result,
            Err(SandboxError::ConcurrentModification { .. })
        ));
        assert!(changed_permissions.readonly());
        assert_eq!(std::fs::read(&destination)?, b"baseline");
        assert_eq!(temporary_file_count(root.path())?, 0);
        Ok(())
    }

    fn temporary_file_count(directory: &std::path::Path) -> Result<usize, std::io::Error> {
        Ok(std::fs::read_dir(directory)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".decode.tmp.")
            })
            .count())
    }

    #[cfg(unix)]
    #[test]
    fn ambient_root_replacement_is_detected() -> Result<(), Box<dyn std::error::Error>> {
        let root = TempDir::new()?;
        let original = root.path().to_path_buf();
        let moved = original.with_extension("opened-workspace");
        let sandbox = SandboxRoot::open(&original)?;

        std::fs::rename(&original, &moved)?;
        std::fs::create_dir(&original)?;
        let result = sandbox.verify_ambient_root_identity();
        std::fs::remove_dir(&original)?;
        std::fs::rename(&moved, &original)?;

        assert!(matches!(result, Err(SandboxError::RootChanged { .. })));
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn held_capability_prevents_ambient_root_replacement() -> Result<(), Box<dyn std::error::Error>>
    {
        let root = TempDir::new()?;
        let original = root.path().to_path_buf();
        let moved = original.with_extension("opened-workspace");
        let sandbox = SandboxRoot::open(&original)?;

        let rename_error = match std::fs::rename(&original, &moved) {
            Ok(()) => {
                return Err("Windows unexpectedly allowed replacement of an opened root".into());
            }
            Err(error) => error,
        };

        assert!(
            matches!(rename_error.raw_os_error(), Some(5 | 32)),
            "unexpected Windows root-replacement error: {rename_error}"
        );
        sandbox.verify_ambient_root_identity()?;
        Ok(())
    }
}
