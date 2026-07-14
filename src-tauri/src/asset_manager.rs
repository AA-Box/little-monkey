//! Transactional manager for app-private trusted sidecars and model assets.
//!
//! Each immutable version lives at
//! `assets/<asset-id>/versions/<version>/` with a versioned manifest, an
//! ownership record, and one regular payload file. Active state is an
//! append-only sequence of fsynced JSON pointer records: publishing one new
//! record atomically changes the selected version without relying on
//! overwrite-style rename semantics that differ between Unix and Windows.
//!
//! This module accepts only a single already-staged regular file or a reader.
//! It never detects or expands archives. A future bounded extractor adapter
//! must own archive entry limits, traversal checks, and decompression budgets
//! before passing one verified regular payload here.

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const ASSET_MANIFEST_VERSION: u32 = 1;
pub const ASSET_STATE_VERSION: u32 = 1;
pub const OWNERSHIP_RECORD_VERSION: u32 = 1;
pub const DEFAULT_ASSET_QUOTA_BYTES: u64 = 20 * 1024 * 1024 * 1024;

const ASSETS_DIRECTORY: &str = "assets";
const TEMP_DIRECTORY: &str = ".tmp";
const VERSIONS_DIRECTORY: &str = "versions";
const STATE_DIRECTORY: &str = "state";
const PAYLOAD_FILE: &str = "payload.bin";
const MANIFEST_FILE: &str = "manifest.json";
const OWNERSHIP_FILE: &str = "ownership.json";
const OWNED_BY: &str = "little-monkey-asset-manager";
const SHA256_HEX_LENGTH: usize = 64;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_IDENTIFIER_BYTES: usize = 128;
const MAX_METADATA_STRING_BYTES: usize = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_STATE_BYTES: u64 = 64 * 1024;

pub type AssetManagerResult<T> = Result<T, AssetManagerError>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Sidecar,
    Model,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetSource {
    pub uri: String,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetProvenance {
    pub publisher: Option<String>,
    pub retrieved_at_ms: Option<u64>,
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetLicense {
    pub name: String,
    pub spdx_id: Option<String>,
    pub url: Option<String>,
    pub text: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetPlatform {
    /// `any` or [`std::env::consts::OS`].
    pub os: String,
    /// `any` or [`std::env::consts::ARCH`].
    pub arch: String,
    /// Optional runtime/accelerator qualifier retained for selection UIs.
    pub variant: Option<String>,
}

impl AssetPlatform {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            variant: None,
        }
    }

    pub fn supports_current_host(&self) -> bool {
        (self.os == "any" || self.os == std::env::consts::OS)
            && (self.arch == "any" || self.arch == std::env::consts::ARCH)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetInstallRequest {
    pub asset_id: String,
    pub kind: AssetKind,
    pub version: String,
    pub source: AssetSource,
    pub provenance: AssetProvenance,
    pub license: AssetLicense,
    pub platform: AssetPlatform,
    pub expected_sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetVersionManifest {
    pub manifest_version: u32,
    pub asset_id: String,
    pub kind: AssetKind,
    pub version: String,
    pub source: AssetSource,
    pub provenance: AssetProvenance,
    pub license: AssetLicense,
    pub platform: AssetPlatform,
    pub sha256: String,
    pub size_bytes: u64,
    pub installed_at_ms: u64,
    pub payload_file: String,
    pub owned_by: String,
    pub ownership_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetStateManifest {
    pub state_version: u32,
    pub asset_id: String,
    pub generation: u64,
    pub active_version: Option<String>,
    pub last_known_good_version: Option<String>,
    pub updated_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct OwnershipRecord {
    record_version: u32,
    owned_by: String,
    ownership_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledAssetVersion {
    pub manifest: AssetVersionManifest,
    pub payload_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveAsset {
    pub state: AssetStateManifest,
    pub version: InstalledAssetVersion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AssetIntegrityStatus {
    Verified,
    Invalid { error: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetVersionStatus {
    pub version: String,
    pub manifest: Option<AssetVersionManifest>,
    pub active: bool,
    pub last_known_good: bool,
    pub integrity: AssetIntegrityStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssetStatus {
    pub asset_id: String,
    pub active_version: Option<String>,
    pub last_known_good_version: Option<String>,
    pub used_bytes: u64,
    pub versions: Vec<AssetVersionStatus>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub removed_temp_entries: usize,
    pub removed_state_records: usize,
    pub removed_empty_asset_directories: usize,
    pub skipped_unsafe_entries: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum AssetManagerError {
    InvalidIdentifier {
        field: &'static str,
        value: String,
    },
    InvalidChecksum {
        checksum: String,
    },
    InvalidMetadata {
        field: &'static str,
        message: String,
    },
    ChecksumMismatch {
        expected: String,
        actual: String,
    },
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    QuotaExceeded {
        quota: u64,
        used: u64,
        requested: u64,
    },
    AssetNotFound {
        asset_id: String,
    },
    VersionNotFound {
        asset_id: String,
        version: String,
    },
    VersionConflict {
        asset_id: String,
        version: String,
    },
    VersionInUse {
        asset_id: String,
        version: String,
    },
    NoRollbackVersion {
        asset_id: String,
    },
    UnownedVersion {
        asset_id: String,
        version: String,
    },
    IncompatiblePlatform {
        asset_id: String,
        version: String,
        os: String,
        arch: String,
    },
    ArchiveExtractionUnsupported,
    UnsafeFileType {
        path: PathBuf,
    },
    SourceChanged {
        path: PathBuf,
    },
    CorruptManifest {
        path: PathBuf,
        message: String,
    },
    LockPoisoned,
    InputRead {
        source: io::Error,
    },
    Json {
        operation: &'static str,
        path: PathBuf,
        source: serde_json::Error,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for AssetManagerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIdentifier { field, value } => {
                write!(f, "invalid {field} identifier: {value:?}")
            }
            Self::InvalidChecksum { checksum } => {
                write!(
                    f,
                    "expected checksum is not lowercase SHA-256: {checksum:?}"
                )
            }
            Self::InvalidMetadata { field, message } => {
                write!(f, "invalid {field} metadata: {message}")
            }
            Self::ChecksumMismatch { expected, actual } => write!(
                f,
                "asset checksum mismatch (expected {expected}, content hashes to {actual})"
            ),
            Self::SizeMismatch { expected, actual } => {
                write!(
                    f,
                    "asset size mismatch (expected {expected} bytes, read {actual})"
                )
            }
            Self::QuotaExceeded {
                quota,
                used,
                requested,
            } => write!(
                f,
                "asset quota exceeded ({used} used + {requested} requested > {quota})"
            ),
            Self::AssetNotFound { asset_id } => write!(f, "asset {asset_id} is not installed"),
            Self::VersionNotFound { asset_id, version } => {
                write!(f, "asset {asset_id} version {version} is not installed")
            }
            Self::VersionConflict { asset_id, version } => write!(
                f,
                "asset {asset_id} version {version} already exists with different metadata"
            ),
            Self::VersionInUse { asset_id, version } => write!(
                f,
                "asset {asset_id} version {version} is active or last-known-good"
            ),
            Self::NoRollbackVersion { asset_id } => {
                write!(
                    f,
                    "asset {asset_id} has no last-known-good rollback version"
                )
            }
            Self::UnownedVersion { asset_id, version } => write!(
                f,
                "refusing to remove unowned asset {asset_id} version {version}"
            ),
            Self::IncompatiblePlatform {
                asset_id,
                version,
                os,
                arch,
            } => write!(
                f,
                "asset {asset_id} version {version} targets {os}/{arch}, not this host"
            ),
            Self::ArchiveExtractionUnsupported => write!(
                f,
                "archive extraction is unsupported until a bounded extractor adapter is configured"
            ),
            Self::UnsafeFileType { path } => write!(
                f,
                "refusing to follow a symlink or use a non-regular asset path: {}",
                path.display()
            ),
            Self::SourceChanged { path } => {
                write!(f, "asset source changed during import: {}", path.display())
            }
            Self::CorruptManifest { path, message } => {
                write!(f, "corrupt asset metadata at {}: {message}", path.display())
            }
            Self::LockPoisoned => write!(f, "asset manager operation lock is poisoned"),
            Self::InputRead { source } => write!(f, "failed to read staged asset input: {source}"),
            Self::Json {
                operation,
                path,
                source,
            } => {
                write!(f, "failed to {operation} {}: {source}", path.display())
            }
            Self::Io {
                operation,
                path,
                source,
            } => {
                write!(f, "failed to {operation} {}: {source}", path.display())
            }
        }
    }
}

impl Error for AssetManagerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InputRead { source } | Self::Io { source, .. } => Some(source),
            Self::Json { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AssetManager {
    root: PathBuf,
    assets_dir: PathBuf,
    temp_dir: PathBuf,
    quota_bytes: u64,
    operation_lock: Arc<Mutex<()>>,
}

impl AssetManager {
    pub fn new(root: impl AsRef<Path>, quota_bytes: u64) -> AssetManagerResult<Self> {
        let root = root.as_ref().to_path_buf();
        create_store_root(&root)?;
        let assets_dir = root.join(ASSETS_DIRECTORY);
        let temp_dir = root.join(TEMP_DIRECTORY);
        ensure_directory(&assets_dir)?;
        ensure_directory(&temp_dir)?;
        Ok(Self {
            root,
            assets_dir,
            temp_dir,
            quota_bytes,
            operation_lock: Arc::new(Mutex::new(())),
        })
    }

    pub fn with_default_quota(root: impl AsRef<Path>) -> AssetManagerResult<Self> {
        Self::new(root, DEFAULT_ASSET_QUOTA_BYTES)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn quota_bytes(&self) -> u64 {
        self.quota_bytes
    }

    pub fn used_bytes(&self) -> AssetManagerResult<u64> {
        let _guard = self.lock()?;
        self.used_bytes_locked()
    }

    pub fn install_reader<R: Read>(
        &self,
        request: &AssetInstallRequest,
        reader: R,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        let _guard = self.lock()?;
        self.install_reader_locked(request, reader)
    }

    pub fn install_file(
        &self,
        request: &AssetInstallRequest,
        source: impl AsRef<Path>,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        let _guard = self.lock()?;
        self.install_file_locked(request, source.as_ref())
    }

    /// Installs and activates a new version while holding one operation lock.
    /// Any validation, checksum, quota, or platform failure occurs before a
    /// new active-state record is published.
    pub fn upgrade_reader<R: Read>(
        &self,
        request: &AssetInstallRequest,
        reader: R,
    ) -> AssetManagerResult<ActiveAsset> {
        let _guard = self.lock()?;
        self.install_reader_locked(request, reader)?;
        self.activate_locked(&request.asset_id, &request.version)
    }

    pub fn upgrade_file(
        &self,
        request: &AssetInstallRequest,
        source: impl AsRef<Path>,
    ) -> AssetManagerResult<ActiveAsset> {
        let _guard = self.lock()?;
        self.install_file_locked(request, source.as_ref())?;
        self.activate_locked(&request.asset_id, &request.version)
    }

    /// Explicitly unsupported: this method never opens or unpacks `archive`.
    pub fn install_archive_file(
        &self,
        _request: &AssetInstallRequest,
        _archive: impl AsRef<Path>,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        Err(AssetManagerError::ArchiveExtractionUnsupported)
    }

    pub fn activate(&self, asset_id: &str, version: &str) -> AssetManagerResult<ActiveAsset> {
        let _guard = self.lock()?;
        self.activate_locked(asset_id, version)
    }

    pub fn rollback(&self, asset_id: &str) -> AssetManagerResult<ActiveAsset> {
        let _guard = self.lock()?;
        validate_identifier("asset_id", asset_id)?;
        let asset_dir = self.asset_dir(asset_id)?;
        if !safe_directory_exists(&asset_dir)? {
            return Err(AssetManagerError::AssetNotFound {
                asset_id: asset_id.to_string(),
            });
        }
        let state = self.load_state(asset_id, &asset_dir)?;
        let target = state.last_known_good_version.clone().ok_or_else(|| {
            AssetManagerError::NoRollbackVersion {
                asset_id: asset_id.to_string(),
            }
        })?;
        self.activate_locked(asset_id, &target)
    }

    /// Promotes the currently active, checksum-verified version to the
    /// last-known-good rollback point after its owning runtime health check.
    pub fn mark_active_last_known_good(&self, asset_id: &str) -> AssetManagerResult<ActiveAsset> {
        let _guard = self.lock()?;
        validate_identifier("asset_id", asset_id)?;
        let asset_dir = self.asset_dir(asset_id)?;
        let state = self.load_state(asset_id, &asset_dir)?;
        let active =
            state
                .active_version
                .clone()
                .ok_or_else(|| AssetManagerError::AssetNotFound {
                    asset_id: asset_id.to_string(),
                })?;
        let installed = self.verify_version(asset_id, &active)?;
        ensure_platform_compatible(&installed.manifest)?;
        if state.last_known_good_version.as_deref() == Some(active.as_str()) {
            return Ok(ActiveAsset {
                state,
                version: installed,
            });
        }
        let next = AssetStateManifest {
            state_version: ASSET_STATE_VERSION,
            asset_id: asset_id.to_string(),
            generation: next_generation(state.generation, &asset_dir)?,
            active_version: Some(active.clone()),
            last_known_good_version: Some(active),
            updated_at_ms: now_ms(),
        };
        self.write_state(&asset_dir, &next)?;
        Ok(ActiveAsset {
            state: next,
            version: installed,
        })
    }

    pub fn status(&self, asset_id: &str) -> AssetManagerResult<Option<AssetStatus>> {
        let _guard = self.lock()?;
        self.status_locked(asset_id)
    }

    pub fn list(&self) -> AssetManagerResult<Vec<AssetStatus>> {
        let _guard = self.lock()?;
        let mut statuses = Vec::new();
        for entry in read_directory(&self.assets_dir, "list managed assets")? {
            let entry = entry
                .map_err(|error| io_at("read managed asset entry", &self.assets_dir, error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| io_at("inspect managed asset entry", &path, error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                return Err(AssetManagerError::UnsafeFileType { path });
            }
            let asset_id = entry
                .file_name()
                .to_str()
                .ok_or_else(|| AssetManagerError::InvalidIdentifier {
                    field: "asset_id",
                    value: "<non-utf8>".to_string(),
                })?
                .to_string();
            validate_identifier("asset_id", &asset_id)?;
            if let Some(status) = self.status_locked(&asset_id)? {
                statuses.push(status);
            }
        }
        statuses.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
        Ok(statuses)
    }

    pub fn remove_version(&self, asset_id: &str, version: &str) -> AssetManagerResult<bool> {
        let _guard = self.lock()?;
        validate_identifier("asset_id", asset_id)?;
        validate_identifier("version", version)?;
        let asset_dir = self.asset_dir(asset_id)?;
        if !safe_directory_exists(&asset_dir)? {
            return Ok(false);
        }
        let state = self.load_state(asset_id, &asset_dir)?;
        if state.active_version.as_deref() == Some(version)
            || state.last_known_good_version.as_deref() == Some(version)
        {
            return Err(AssetManagerError::VersionInUse {
                asset_id: asset_id.to_string(),
                version: version.to_string(),
            });
        }
        let version_dir = self.version_dir(asset_id, version)?;
        match fs::symlink_metadata(&version_dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_at("inspect managed asset version", &version_dir, error)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AssetManagerError::UnsafeFileType { path: version_dir });
            }
            Ok(_) => {}
        }
        self.verify_owned_layout(asset_id, version, &version_dir)?;
        fs::remove_dir_all(&version_dir)
            .map_err(|error| io_at("remove managed asset version", &version_dir, error))?;
        if let Some(parent) = version_dir.parent() {
            sync_directory(parent)?;
        }
        Ok(true)
    }

    pub fn cleanup_orphans(&self) -> AssetManagerResult<CleanupReport> {
        let _guard = self.lock()?;
        let mut report = CleanupReport::default();
        self.cleanup_global_temp(&mut report)?;
        self.cleanup_asset_state(&mut report)?;
        Ok(report)
    }

    fn lock(&self) -> AssetManagerResult<MutexGuard<'_, ()>> {
        self.operation_lock
            .lock()
            .map_err(|_| AssetManagerError::LockPoisoned)
    }

    fn asset_dir(&self, asset_id: &str) -> AssetManagerResult<PathBuf> {
        validate_identifier("asset_id", asset_id)?;
        Ok(self.assets_dir.join(asset_id))
    }

    fn version_dir(&self, asset_id: &str, version: &str) -> AssetManagerResult<PathBuf> {
        validate_identifier("asset_id", asset_id)?;
        validate_identifier("version", version)?;
        Ok(self
            .assets_dir
            .join(asset_id)
            .join(VERSIONS_DIRECTORY)
            .join(version))
    }

    fn install_reader_locked<R: Read>(
        &self,
        request: &AssetInstallRequest,
        reader: R,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        if let Some(existing) = self.prepare_install(request)? {
            return Ok(existing);
        }
        let staged = self.stage_install(request, reader)?;
        self.publish_install(staged)
    }

    fn install_file_locked(
        &self,
        request: &AssetInstallRequest,
        source: &Path,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        if let Some(existing) = self.prepare_install(request)? {
            return Ok(existing);
        }
        let before = metadata_regular_no_symlink(source, "inspect staged asset file")?;
        if before.len() != request.size_bytes {
            return Err(AssetManagerError::SizeMismatch {
                expected: request.size_bytes,
                actual: before.len(),
            });
        }
        let mut file =
            File::open(source).map_err(|error| io_at("open staged asset file", source, error))?;
        let opened = file
            .metadata()
            .map_err(|error| io_at("inspect opened staged asset file", source, error))?;
        if !opened.is_file() || !same_file_identity(&before, &opened) {
            return Err(AssetManagerError::SourceChanged {
                path: source.to_path_buf(),
            });
        }
        let staged = self.stage_install(request, &mut file)?;
        let after = file
            .metadata()
            .map_err(|error| io_at("reinspect staged asset file", source, error))?;
        if !same_file_snapshot(&opened, &after) || staged.manifest.size_bytes != opened.len() {
            return Err(AssetManagerError::SourceChanged {
                path: source.to_path_buf(),
            });
        }
        self.publish_install(staged)
    }

    /// Returns an idempotent existing install, or reserves quota logically by
    /// validating that the requested size still fits before any bytes stage.
    fn prepare_install(
        &self,
        request: &AssetInstallRequest,
    ) -> AssetManagerResult<Option<InstalledAssetVersion>> {
        validate_install_request(request)?;
        let version_dir = self.version_dir(&request.asset_id, &request.version)?;
        match fs::symlink_metadata(&version_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AssetManagerError::UnsafeFileType { path: version_dir });
            }
            Ok(_) => {
                let existing = self.verify_version(&request.asset_id, &request.version)?;
                if manifest_matches_request(&existing.manifest, request) {
                    return Ok(Some(existing));
                }
                return Err(AssetManagerError::VersionConflict {
                    asset_id: request.asset_id.clone(),
                    version: request.version.clone(),
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(io_at(
                    "inspect asset install destination",
                    &version_dir,
                    error,
                ))
            }
        }
        let used = self.used_bytes_locked()?;
        if used.saturating_add(request.size_bytes) > self.quota_bytes {
            return Err(AssetManagerError::QuotaExceeded {
                quota: self.quota_bytes,
                used,
                requested: request.size_bytes,
            });
        }
        Ok(None)
    }

    fn stage_install<R: Read>(
        &self,
        request: &AssetInstallRequest,
        mut reader: R,
    ) -> AssetManagerResult<StagedInstall> {
        let stage = self.create_stage_directory()?;
        let payload_path = stage.path().join(PAYLOAD_FILE);
        let mut payload = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&payload_path)
            .map_err(|error| io_at("create staged asset payload", &payload_path, error))?;
        let outcome = (|| -> AssetManagerResult<(String, u64)> {
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; COPY_BUFFER_BYTES];
            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|source| AssetManagerError::InputRead { source })?;
                if read == 0 {
                    break;
                }
                total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
                if total > request.size_bytes {
                    return Err(AssetManagerError::SizeMismatch {
                        expected: request.size_bytes,
                        actual: total,
                    });
                }
                payload
                    .write_all(&buffer[..read])
                    .map_err(|error| io_at("write staged asset payload", &payload_path, error))?;
                hasher.update(&buffer[..read]);
            }
            payload
                .sync_all()
                .map_err(|error| io_at("flush staged asset payload", &payload_path, error))?;
            Ok((digest_hex(hasher.finalize().as_slice()), total))
        })();
        drop(payload);
        let (actual_sha256, actual_size) = outcome?;
        if actual_size != request.size_bytes {
            return Err(AssetManagerError::SizeMismatch {
                expected: request.size_bytes,
                actual: actual_size,
            });
        }
        if actual_sha256 != request.expected_sha256 {
            return Err(AssetManagerError::ChecksumMismatch {
                expected: request.expected_sha256.clone(),
                actual: actual_sha256,
            });
        }

        let ownership_id = Uuid::new_v4().to_string();
        let manifest = AssetVersionManifest {
            manifest_version: ASSET_MANIFEST_VERSION,
            asset_id: request.asset_id.clone(),
            kind: request.kind.clone(),
            version: request.version.clone(),
            source: request.source.clone(),
            provenance: request.provenance.clone(),
            license: request.license.clone(),
            platform: request.platform.clone(),
            sha256: actual_sha256,
            size_bytes: actual_size,
            installed_at_ms: now_ms(),
            payload_file: PAYLOAD_FILE.to_string(),
            owned_by: OWNED_BY.to_string(),
            ownership_id: ownership_id.clone(),
        };
        let ownership = OwnershipRecord {
            record_version: OWNERSHIP_RECORD_VERSION,
            owned_by: OWNED_BY.to_string(),
            ownership_id,
        };
        write_json_new(
            &stage.path().join(MANIFEST_FILE),
            &manifest,
            MAX_MANIFEST_BYTES,
        )?;
        write_json_new(
            &stage.path().join(OWNERSHIP_FILE),
            &ownership,
            MAX_STATE_BYTES,
        )?;
        sync_directory(stage.path())?;
        Ok(StagedInstall { stage, manifest })
    }

    fn create_stage_directory(&self) -> AssetManagerResult<StageDirectory> {
        for _ in 0..16 {
            let path = self
                .temp_dir
                .join(format!("stage-{}", Uuid::new_v4().simple()));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(StageDirectory::new(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_at("create asset staging directory", &path, error)),
            }
        }
        Err(io_at(
            "create asset staging directory",
            &self.temp_dir,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate unique stage",
            ),
        ))
    }

    fn publish_install(
        &self,
        mut staged: StagedInstall,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        let asset_dir = self.asset_dir(&staged.manifest.asset_id)?;
        ensure_directory(&asset_dir)?;
        let versions_dir = asset_dir.join(VERSIONS_DIRECTORY);
        ensure_directory(&versions_dir)?;
        let destination = versions_dir.join(&staged.manifest.version);

        if let Err(rename_error) = fs::rename(staged.stage.path(), &destination) {
            match self.verify_version(&staged.manifest.asset_id, &staged.manifest.version) {
                Ok(existing) if manifests_equivalent(&existing.manifest, &staged.manifest) => {
                    return Ok(existing);
                }
                Ok(_) => {
                    return Err(AssetManagerError::VersionConflict {
                        asset_id: staged.manifest.asset_id,
                        version: staged.manifest.version,
                    });
                }
                Err(AssetManagerError::VersionNotFound { .. }) => {
                    return Err(io_at("publish asset version", &destination, rename_error));
                }
                Err(error) => return Err(error),
            }
        }
        staged.stage.disarm();
        sync_directory(&versions_dir)?;
        sync_directory(&self.temp_dir)?;
        self.verify_version(&staged.manifest.asset_id, &staged.manifest.version)
    }

    fn activate_locked(&self, asset_id: &str, version: &str) -> AssetManagerResult<ActiveAsset> {
        validate_identifier("asset_id", asset_id)?;
        validate_identifier("version", version)?;
        let installed = self.verify_version(asset_id, version)?;
        ensure_platform_compatible(&installed.manifest)?;
        let asset_dir = self.asset_dir(asset_id)?;
        let state = self.load_state(asset_id, &asset_dir)?;

        // Re-activation is still a full checksum verification, but does not
        // create a redundant pointer generation.
        if state.active_version.as_deref() == Some(version) {
            return Ok(ActiveAsset {
                state,
                version: installed,
            });
        }

        let last_known_good_version = match state.last_known_good_version.clone() {
            Some(version) => Some(version),
            None => match state.active_version.as_deref() {
                Some(current) if self.verify_version(asset_id, current).is_ok() => {
                    Some(current.to_string())
                }
                _ => Some(version.to_string()),
            },
        };
        let next = AssetStateManifest {
            state_version: ASSET_STATE_VERSION,
            asset_id: asset_id.to_string(),
            generation: next_generation(state.generation, &asset_dir)?,
            active_version: Some(version.to_string()),
            last_known_good_version,
            updated_at_ms: now_ms(),
        };
        self.write_state(&asset_dir, &next)?;
        Ok(ActiveAsset {
            state: next,
            version: installed,
        })
    }

    fn verify_version(
        &self,
        asset_id: &str,
        version: &str,
    ) -> AssetManagerResult<InstalledAssetVersion> {
        let version_dir = self.version_dir(asset_id, version)?;
        match fs::symlink_metadata(&version_dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(AssetManagerError::VersionNotFound {
                    asset_id: asset_id.to_string(),
                    version: version.to_string(),
                });
            }
            Err(error) => return Err(io_at("inspect managed asset version", &version_dir, error)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AssetManagerError::UnsafeFileType { path: version_dir });
            }
            Ok(_) => {}
        }
        let manifest = self.load_manifest(asset_id, version, &version_dir)?;
        self.verify_ownership(asset_id, version, &version_dir, &manifest)?;
        let payload_path = version_dir.join(PAYLOAD_FILE);
        verify_payload(&payload_path, &manifest)?;
        Ok(InstalledAssetVersion {
            manifest,
            payload_path,
        })
    }

    fn load_manifest(
        &self,
        asset_id: &str,
        version: &str,
        version_dir: &Path,
    ) -> AssetManagerResult<AssetVersionManifest> {
        let path = version_dir.join(MANIFEST_FILE);
        let manifest: AssetVersionManifest = read_json_bounded(&path, MAX_MANIFEST_BYTES)?;
        validate_manifest(&manifest, &path)?;
        if manifest.asset_id != asset_id || manifest.version != version {
            return Err(AssetManagerError::CorruptManifest {
                path,
                message: "manifest identity does not match its directory".to_string(),
            });
        }
        Ok(manifest)
    }

    fn verify_ownership(
        &self,
        asset_id: &str,
        version: &str,
        version_dir: &Path,
        manifest: &AssetVersionManifest,
    ) -> AssetManagerResult<()> {
        let path = version_dir.join(OWNERSHIP_FILE);
        let ownership: OwnershipRecord =
            read_json_bounded(&path, MAX_STATE_BYTES).map_err(|_| {
                AssetManagerError::UnownedVersion {
                    asset_id: asset_id.to_string(),
                    version: version.to_string(),
                }
            })?;
        if ownership.record_version != OWNERSHIP_RECORD_VERSION
            || ownership.owned_by != OWNED_BY
            || manifest.owned_by != OWNED_BY
            || ownership.ownership_id != manifest.ownership_id
        {
            return Err(AssetManagerError::UnownedVersion {
                asset_id: asset_id.to_string(),
                version: version.to_string(),
            });
        }
        Ok(())
    }

    fn verify_owned_layout(
        &self,
        asset_id: &str,
        version: &str,
        version_dir: &Path,
    ) -> AssetManagerResult<()> {
        let manifest = self.load_manifest(asset_id, version, version_dir)?;
        self.verify_ownership(asset_id, version, version_dir, &manifest)?;
        let mut seen = std::collections::BTreeSet::new();
        for entry in read_directory(version_dir, "inspect owned asset layout")? {
            let entry =
                entry.map_err(|error| io_at("read owned asset entry", version_dir, error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| io_at("inspect owned asset entry", &path, error))?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(AssetManagerError::UnsafeFileType { path });
            }
            let name = entry
                .file_name()
                .to_str()
                .ok_or_else(|| AssetManagerError::UnownedVersion {
                    asset_id: asset_id.to_string(),
                    version: version.to_string(),
                })?
                .to_string();
            if ![PAYLOAD_FILE, MANIFEST_FILE, OWNERSHIP_FILE].contains(&name.as_str()) {
                return Err(AssetManagerError::UnownedVersion {
                    asset_id: asset_id.to_string(),
                    version: version.to_string(),
                });
            }
            seen.insert(name);
        }
        if ![PAYLOAD_FILE, MANIFEST_FILE, OWNERSHIP_FILE]
            .iter()
            .all(|name| seen.contains(*name))
        {
            return Err(AssetManagerError::UnownedVersion {
                asset_id: asset_id.to_string(),
                version: version.to_string(),
            });
        }
        Ok(())
    }

    fn load_state(
        &self,
        asset_id: &str,
        asset_dir: &Path,
    ) -> AssetManagerResult<AssetStateManifest> {
        let records = self.read_state_records(asset_id, asset_dir)?;
        Ok(records
            .into_iter()
            .max_by_key(|(_, state)| state.generation)
            .map(|(_, state)| state)
            .unwrap_or_else(|| empty_state(asset_id)))
    }

    fn read_state_records(
        &self,
        asset_id: &str,
        asset_dir: &Path,
    ) -> AssetManagerResult<Vec<(PathBuf, AssetStateManifest)>> {
        let state_dir = asset_dir.join(STATE_DIRECTORY);
        match fs::symlink_metadata(&state_dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_at("inspect asset state directory", &state_dir, error)),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AssetManagerError::UnsafeFileType { path: state_dir });
            }
            Ok(_) => {}
        }
        let mut records = Vec::new();
        let mut generations = std::collections::BTreeSet::new();
        for entry in read_directory(&state_dir, "read asset state records")? {
            let entry =
                entry.map_err(|error| io_at("read asset state entry", &state_dir, error))?;
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| io_at("inspect asset state entry", &path, error))?;
            if file_type.is_symlink() || !file_type.is_file() {
                return Err(AssetManagerError::UnsafeFileType { path });
            }
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| AssetManagerError::CorruptManifest {
                    path: path.clone(),
                    message: "state filename is not UTF-8".to_string(),
                })?;
            if is_state_temp_name(name) {
                continue;
            }
            let generation = parse_state_record_name(name).ok_or_else(|| {
                AssetManagerError::CorruptManifest {
                    path: path.clone(),
                    message: "unexpected active-state record filename".to_string(),
                }
            })?;
            let state: AssetStateManifest = read_json_bounded(&path, MAX_STATE_BYTES)?;
            validate_state(&state, asset_id, generation, &path)?;
            if !generations.insert(generation) {
                return Err(AssetManagerError::CorruptManifest {
                    path,
                    message: format!("duplicate active-state generation {generation}"),
                });
            }
            records.push((entry.path(), state));
        }
        Ok(records)
    }

    fn write_state(&self, asset_dir: &Path, state: &AssetStateManifest) -> AssetManagerResult<()> {
        let state_dir = asset_dir.join(STATE_DIRECTORY);
        ensure_directory(&state_dir)?;
        let uuid = Uuid::new_v4();
        let temp_path = state_dir.join(format!(".state-{}.tmp", uuid.simple()));
        let final_path = state_dir.join(format!(
            "state-{:020}-{}.json",
            state.generation,
            uuid.simple()
        ));
        let mut temp = TempFileGuard::new(temp_path.clone());
        write_json_new(&temp_path, state, MAX_STATE_BYTES)?;
        fs::rename(&temp_path, &final_path)
            .map_err(|error| io_at("publish active asset pointer", &final_path, error))?;
        temp.disarm();
        sync_directory(&state_dir)
    }

    fn status_locked(&self, asset_id: &str) -> AssetManagerResult<Option<AssetStatus>> {
        validate_identifier("asset_id", asset_id)?;
        let asset_dir = self.asset_dir(asset_id)?;
        if !safe_directory_exists(&asset_dir)? {
            return Ok(None);
        }
        let state = self.load_state(asset_id, &asset_dir)?;
        let versions_dir = asset_dir.join(VERSIONS_DIRECTORY);
        let mut versions = Vec::new();
        let mut used_bytes = 0_u64;
        if safe_directory_exists(&versions_dir)? {
            for entry in read_directory(&versions_dir, "list asset versions")? {
                let entry = entry
                    .map_err(|error| io_at("read asset version entry", &versions_dir, error))?;
                let path = entry.path();
                let file_type = entry
                    .file_type()
                    .map_err(|error| io_at("inspect asset version entry", &path, error))?;
                if file_type.is_symlink() || !file_type.is_dir() {
                    return Err(AssetManagerError::UnsafeFileType { path });
                }
                let version = entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| AssetManagerError::InvalidIdentifier {
                        field: "version",
                        value: "<non-utf8>".to_string(),
                    })?
                    .to_string();
                validate_identifier("version", &version)?;
                let payload_path = path.join(PAYLOAD_FILE);
                if let Ok(metadata) = fs::symlink_metadata(&payload_path) {
                    if metadata.file_type().is_symlink() || !metadata.is_file() {
                        return Err(AssetManagerError::UnsafeFileType { path: payload_path });
                    }
                    used_bytes = used_bytes.saturating_add(metadata.len());
                }
                let verified = self.verify_version(asset_id, &version);
                let (manifest, integrity) = match verified {
                    Ok(installed) => (Some(installed.manifest), AssetIntegrityStatus::Verified),
                    Err(error) => {
                        let manifest = self.load_manifest(asset_id, &version, &path).ok();
                        (
                            manifest,
                            AssetIntegrityStatus::Invalid {
                                error: error.to_string(),
                            },
                        )
                    }
                };
                versions.push(AssetVersionStatus {
                    version: version.clone(),
                    manifest,
                    active: state.active_version.as_deref() == Some(version.as_str()),
                    last_known_good: state.last_known_good_version.as_deref()
                        == Some(version.as_str()),
                    integrity,
                });
            }
        }
        versions.sort_by(|left, right| left.version.cmp(&right.version));
        Ok(Some(AssetStatus {
            asset_id: asset_id.to_string(),
            active_version: state.active_version,
            last_known_good_version: state.last_known_good_version,
            used_bytes,
            versions,
        }))
    }

    fn used_bytes_locked(&self) -> AssetManagerResult<u64> {
        let mut total = 0_u64;
        for asset_entry in read_directory(&self.assets_dir, "calculate asset quota usage")? {
            let asset_entry = asset_entry
                .map_err(|error| io_at("read asset quota entry", &self.assets_dir, error))?;
            let asset_path = asset_entry.path();
            let asset_type = asset_entry
                .file_type()
                .map_err(|error| io_at("inspect asset quota entry", &asset_path, error))?;
            if asset_type.is_symlink() || !asset_type.is_dir() {
                return Err(AssetManagerError::UnsafeFileType { path: asset_path });
            }
            let asset_id = asset_entry
                .file_name()
                .to_str()
                .ok_or_else(|| AssetManagerError::InvalidIdentifier {
                    field: "asset_id",
                    value: "<non-utf8>".to_string(),
                })?
                .to_string();
            validate_identifier("asset_id", &asset_id)?;
            let versions_dir = asset_path.join(VERSIONS_DIRECTORY);
            if !safe_directory_exists(&versions_dir)? {
                continue;
            }
            for version_entry in read_directory(&versions_dir, "calculate version quota usage")? {
                let version_entry = version_entry
                    .map_err(|error| io_at("read version quota entry", &versions_dir, error))?;
                let version_path = version_entry.path();
                let version_type = version_entry
                    .file_type()
                    .map_err(|error| io_at("inspect version quota entry", &version_path, error))?;
                if version_type.is_symlink() || !version_type.is_dir() {
                    return Err(AssetManagerError::UnsafeFileType { path: version_path });
                }
                let version = version_entry
                    .file_name()
                    .to_str()
                    .ok_or_else(|| AssetManagerError::InvalidIdentifier {
                        field: "version",
                        value: "<non-utf8>".to_string(),
                    })?
                    .to_string();
                validate_identifier("version", &version)?;
                let payload = version_path.join(PAYLOAD_FILE);
                match fs::symlink_metadata(&payload) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        return Err(AssetManagerError::UnsafeFileType { path: payload });
                    }
                    Ok(metadata) => total = total.saturating_add(metadata.len()),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(io_at("inspect asset payload usage", &payload, error))
                    }
                }
            }
        }
        Ok(total)
    }

    fn cleanup_global_temp(&self, report: &mut CleanupReport) -> AssetManagerResult<()> {
        for entry in read_directory(&self.temp_dir, "scan asset temporary directory")? {
            let entry = entry
                .map_err(|error| io_at("read asset temporary entry", &self.temp_dir, error))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_str().unwrap_or_default();
            let file_type = entry
                .file_type()
                .map_err(|error| io_at("inspect asset temporary entry", &path, error))?;
            if !valid_stage_name(name) || file_type.is_symlink() || !file_type.is_dir() {
                report.skipped_unsafe_entries.push(path);
                continue;
            }
            if !tree_is_symlink_free(&path)? {
                report.skipped_unsafe_entries.push(path);
                continue;
            }
            fs::remove_dir_all(&path)
                .map_err(|error| io_at("remove orphaned asset stage", &path, error))?;
            report.removed_temp_entries += 1;
        }
        sync_directory(&self.temp_dir)
    }

    fn cleanup_asset_state(&self, report: &mut CleanupReport) -> AssetManagerResult<()> {
        let asset_entries: Vec<_> =
            read_directory(&self.assets_dir, "scan asset state cleanup")?.collect();
        for entry in asset_entries {
            let entry = entry
                .map_err(|error| io_at("read asset cleanup entry", &self.assets_dir, error))?;
            let asset_dir = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| io_at("inspect asset cleanup entry", &asset_dir, error))?;
            if file_type.is_symlink() || !file_type.is_dir() {
                report.skipped_unsafe_entries.push(asset_dir);
                continue;
            }
            let Some(asset_id) = entry.file_name().to_str().map(str::to_owned) else {
                report.skipped_unsafe_entries.push(asset_dir);
                continue;
            };
            if validate_identifier("asset_id", &asset_id).is_err() {
                report.skipped_unsafe_entries.push(asset_dir);
                continue;
            }

            let state_dir = asset_dir.join(STATE_DIRECTORY);
            if safe_directory_exists(&state_dir)? {
                let state_entries: Vec<_> =
                    read_directory(&state_dir, "scan state temporary files")?.collect();
                for state_entry in state_entries {
                    let state_entry = state_entry
                        .map_err(|error| io_at("read state cleanup entry", &state_dir, error))?;
                    let path = state_entry.path();
                    let file_type = state_entry
                        .file_type()
                        .map_err(|error| io_at("inspect state cleanup entry", &path, error))?;
                    let name = state_entry.file_name();
                    let name = name.to_str().unwrap_or_default();
                    if is_state_temp_name(name) && file_type.is_file() && !file_type.is_symlink() {
                        fs::remove_file(&path)
                            .map_err(|error| io_at("remove orphaned state temp", &path, error))?;
                        report.removed_temp_entries += 1;
                    }
                }
                let records = self.read_state_records(&asset_id, &asset_dir)?;
                let latest_generation = records.iter().map(|(_, state)| state.generation).max();
                for (path, state) in records {
                    if Some(state.generation) != latest_generation {
                        fs::remove_file(&path)
                            .map_err(|error| io_at("remove obsolete state record", &path, error))?;
                        report.removed_state_records += 1;
                    }
                }
                sync_directory(&state_dir)?;
            }

            let versions_dir = asset_dir.join(VERSIONS_DIRECTORY);
            let versions_empty = !safe_directory_exists(&versions_dir)?
                || read_directory(&versions_dir, "inspect empty versions directory")?
                    .next()
                    .is_none();
            let state_empty = !safe_directory_exists(&state_dir)?
                || read_directory(&state_dir, "inspect empty state directory")?
                    .next()
                    .is_none();
            if versions_empty
                && state_empty
                && directory_has_only(&asset_dir, &[VERSIONS_DIRECTORY, STATE_DIRECTORY])?
            {
                if safe_directory_exists(&versions_dir)? {
                    fs::remove_dir(&versions_dir).map_err(|error| {
                        io_at("remove empty versions directory", &versions_dir, error)
                    })?;
                }
                if safe_directory_exists(&state_dir)? {
                    fs::remove_dir(&state_dir).map_err(|error| {
                        io_at("remove empty state directory", &state_dir, error)
                    })?;
                }
                fs::remove_dir(&asset_dir)
                    .map_err(|error| io_at("remove empty asset directory", &asset_dir, error))?;
                report.removed_empty_asset_directories += 1;
            }
        }
        sync_directory(&self.assets_dir)
    }
}

struct StagedInstall {
    stage: StageDirectory,
    manifest: AssetVersionManifest,
}

struct StageDirectory {
    path: PathBuf,
    armed: bool,
}

impl StageDirectory {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StageDirectory {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct TempFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_install_request(request: &AssetInstallRequest) -> AssetManagerResult<()> {
    validate_identifier("asset_id", &request.asset_id)?;
    validate_identifier("version", &request.version)?;
    validate_sha256(&request.expected_sha256)?;
    validate_required_metadata("source.uri", &request.source.uri)?;
    validate_optional_metadata("source.revision", request.source.revision.as_deref())?;
    validate_optional_metadata(
        "provenance.publisher",
        request.provenance.publisher.as_deref(),
    )?;
    validate_optional_metadata("provenance.notes", request.provenance.notes.as_deref())?;
    validate_required_metadata("license.name", &request.license.name)?;
    validate_optional_metadata("license.spdx_id", request.license.spdx_id.as_deref())?;
    validate_optional_metadata("license.url", request.license.url.as_deref())?;
    validate_optional_metadata("license.text", request.license.text.as_deref())?;
    validate_required_metadata("platform.os", &request.platform.os)?;
    validate_required_metadata("platform.arch", &request.platform.arch)?;
    validate_optional_metadata("platform.variant", request.platform.variant.as_deref())?;
    let encoded = serde_json::to_vec(request).map_err(|source| AssetManagerError::Json {
        operation: "serialize asset install request",
        path: PathBuf::from("<request>"),
        source,
    })?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
        return Err(AssetManagerError::InvalidMetadata {
            field: "manifest",
            message: format!("serialized metadata exceeds {MAX_MANIFEST_BYTES} bytes"),
        });
    }
    Ok(())
}

fn validate_manifest(manifest: &AssetVersionManifest, path: &Path) -> AssetManagerResult<()> {
    if manifest.manifest_version != ASSET_MANIFEST_VERSION {
        return Err(corrupt(path, "unsupported asset manifest version"));
    }
    let request = AssetInstallRequest {
        asset_id: manifest.asset_id.clone(),
        kind: manifest.kind.clone(),
        version: manifest.version.clone(),
        source: manifest.source.clone(),
        provenance: manifest.provenance.clone(),
        license: manifest.license.clone(),
        platform: manifest.platform.clone(),
        expected_sha256: manifest.sha256.clone(),
        size_bytes: manifest.size_bytes,
    };
    validate_install_request(&request)
        .map_err(|error| corrupt(path, format!("invalid manifest fields: {error}")))?;
    if manifest.payload_file != PAYLOAD_FILE || manifest.owned_by != OWNED_BY {
        return Err(corrupt(path, "unexpected payload or ownership declaration"));
    }
    if Uuid::parse_str(&manifest.ownership_id).is_err() {
        return Err(corrupt(path, "invalid ownership id"));
    }
    Ok(())
}

fn validate_state(
    state: &AssetStateManifest,
    asset_id: &str,
    generation: u64,
    path: &Path,
) -> AssetManagerResult<()> {
    if state.state_version != ASSET_STATE_VERSION
        || state.asset_id != asset_id
        || state.generation != generation
        || generation == 0
    {
        return Err(corrupt(path, "active-state identity/version mismatch"));
    }
    if let Some(version) = state.active_version.as_deref() {
        validate_identifier("version", version)
            .map_err(|error| corrupt(path, format!("invalid active version: {error}")))?;
    }
    if let Some(version) = state.last_known_good_version.as_deref() {
        validate_identifier("version", version)
            .map_err(|error| corrupt(path, format!("invalid last-known-good version: {error}")))?;
    }
    Ok(())
}

fn validate_identifier(field: &'static str, value: &str) -> AssetManagerResult<()> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && !value.starts_with('.')
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(AssetManagerError::InvalidIdentifier {
            field,
            value: value.to_string(),
        })
    }
}

fn validate_sha256(checksum: &str) -> AssetManagerResult<()> {
    if checksum.len() == SHA256_HEX_LENGTH
        && checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(AssetManagerError::InvalidChecksum {
            checksum: checksum.to_string(),
        })
    }
}

fn validate_required_metadata(field: &'static str, value: &str) -> AssetManagerResult<()> {
    if value.trim().is_empty() {
        return Err(AssetManagerError::InvalidMetadata {
            field,
            message: "must not be empty".to_string(),
        });
    }
    validate_metadata_length(field, value)
}

fn validate_optional_metadata(field: &'static str, value: Option<&str>) -> AssetManagerResult<()> {
    match value {
        Some(value) => validate_metadata_length(field, value),
        None => Ok(()),
    }
}

fn validate_metadata_length(field: &'static str, value: &str) -> AssetManagerResult<()> {
    if value.len() > MAX_METADATA_STRING_BYTES || value.contains('\0') {
        Err(AssetManagerError::InvalidMetadata {
            field,
            message: format!(
                "must be at most {MAX_METADATA_STRING_BYTES} bytes and contain no NUL"
            ),
        })
    } else {
        Ok(())
    }
}

fn manifest_matches_request(
    manifest: &AssetVersionManifest,
    request: &AssetInstallRequest,
) -> bool {
    manifest.asset_id == request.asset_id
        && manifest.kind == request.kind
        && manifest.version == request.version
        && manifest.source == request.source
        && manifest.provenance == request.provenance
        && manifest.license == request.license
        && manifest.platform == request.platform
        && manifest.sha256 == request.expected_sha256
        && manifest.size_bytes == request.size_bytes
}

fn manifests_equivalent(left: &AssetVersionManifest, right: &AssetVersionManifest) -> bool {
    left.asset_id == right.asset_id
        && left.kind == right.kind
        && left.version == right.version
        && left.source == right.source
        && left.provenance == right.provenance
        && left.license == right.license
        && left.platform == right.platform
        && left.sha256 == right.sha256
        && left.size_bytes == right.size_bytes
}

fn ensure_platform_compatible(manifest: &AssetVersionManifest) -> AssetManagerResult<()> {
    if manifest.platform.supports_current_host() {
        Ok(())
    } else {
        Err(AssetManagerError::IncompatiblePlatform {
            asset_id: manifest.asset_id.clone(),
            version: manifest.version.clone(),
            os: manifest.platform.os.clone(),
            arch: manifest.platform.arch.clone(),
        })
    }
}

fn verify_payload(path: &Path, manifest: &AssetVersionManifest) -> AssetManagerResult<()> {
    let before = metadata_regular_no_symlink(path, "inspect managed asset payload")?;
    if before.len() != manifest.size_bytes {
        return Err(AssetManagerError::SizeMismatch {
            expected: manifest.size_bytes,
            actual: before.len(),
        });
    }
    let mut file =
        File::open(path).map_err(|error| io_at("open managed asset payload", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| io_at("inspect opened asset payload", path, error))?;
    if !opened.is_file() || !same_file_identity(&before, &opened) {
        return Err(AssetManagerError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    let mut hasher = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| io_at("verify managed asset payload", path, error))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        if total > manifest.size_bytes {
            return Err(AssetManagerError::SizeMismatch {
                expected: manifest.size_bytes,
                actual: total,
            });
        }
        hasher.update(&buffer[..read]);
    }
    let after = file
        .metadata()
        .map_err(|error| io_at("reinspect managed asset payload", path, error))?;
    if !same_file_snapshot(&opened, &after) || total != manifest.size_bytes {
        return Err(AssetManagerError::SizeMismatch {
            expected: manifest.size_bytes,
            actual: total,
        });
    }
    let actual = digest_hex(hasher.finalize().as_slice());
    if actual != manifest.sha256 {
        return Err(AssetManagerError::ChecksumMismatch {
            expected: manifest.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

fn empty_state(asset_id: &str) -> AssetStateManifest {
    AssetStateManifest {
        state_version: ASSET_STATE_VERSION,
        asset_id: asset_id.to_string(),
        generation: 0,
        active_version: None,
        last_known_good_version: None,
        updated_at_ms: 0,
    }
}

fn next_generation(current: u64, path: &Path) -> AssetManagerResult<u64> {
    current
        .checked_add(1)
        .ok_or_else(|| corrupt(path, "active-state generation overflow"))
}

fn parse_state_record_name(name: &str) -> Option<u64> {
    let body = name.strip_prefix("state-")?.strip_suffix(".json")?;
    let (generation, uuid) = body.split_once('-')?;
    if generation.len() != 20 || Uuid::parse_str(uuid).is_err() {
        return None;
    }
    generation.parse().ok()
}

fn is_state_temp_name(name: &str) -> bool {
    name.strip_prefix(".state-")
        .and_then(|value| value.strip_suffix(".tmp"))
        .is_some_and(|uuid| Uuid::parse_str(uuid).is_ok())
}

fn valid_stage_name(name: &str) -> bool {
    name.strip_prefix("stage-")
        .is_some_and(|uuid| Uuid::parse_str(uuid).is_ok())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn digest_hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex(Sha256::digest(bytes).as_slice())
}

fn create_store_root(path: &Path) -> AssetManagerResult<()> {
    fs::create_dir_all(path).map_err(|error| io_at("create asset manager root", path, error))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io_at("inspect asset manager root", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AssetManagerError::UnsafeFileType {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> AssetManagerResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(AssetManagerError::UnsafeFileType {
                path: path.to_path_buf(),
            })
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => match fs::create_dir(path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent)?;
                }
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => ensure_directory(path),
            Err(error) => Err(io_at("create managed asset directory", path, error)),
        },
        Err(error) => Err(io_at("inspect managed asset directory", path, error)),
    }
}

fn safe_directory_exists(path: &Path) -> AssetManagerResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(AssetManagerError::UnsafeFileType {
                path: path.to_path_buf(),
            })
        }
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_at("inspect managed asset directory", path, error)),
    }
}

fn metadata_regular_no_symlink(
    path: &Path,
    operation: &'static str,
) -> AssetManagerResult<Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_at(operation, path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AssetManagerError::UnsafeFileType {
            path: path.to_path_buf(),
        });
    }
    Ok(metadata)
}

fn read_directory(path: &Path, operation: &'static str) -> AssetManagerResult<fs::ReadDir> {
    fs::read_dir(path).map_err(|error| io_at(operation, path, error))
}

fn write_json_new<T: Serialize>(path: &Path, value: &T, max_bytes: u64) -> AssetManagerResult<()> {
    let encoded = serde_json::to_vec_pretty(value).map_err(|source| AssetManagerError::Json {
        operation: "serialize asset metadata",
        path: path.to_path_buf(),
        source,
    })?;
    let size = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    if size > max_bytes {
        return Err(AssetManagerError::InvalidMetadata {
            field: "manifest",
            message: format!("serialized metadata is {size} bytes; limit is {max_bytes}"),
        });
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| io_at("create asset metadata", path, error))?;
    file.write_all(&encoded)
        .map_err(|error| io_at("write asset metadata", path, error))?;
    file.sync_all()
        .map_err(|error| io_at("flush asset metadata", path, error))
}

fn read_json_bounded<T: DeserializeOwned>(path: &Path, max_bytes: u64) -> AssetManagerResult<T> {
    let before = metadata_regular_no_symlink(path, "inspect asset metadata")?;
    if before.len() > max_bytes {
        return Err(corrupt(path, format!("metadata exceeds {max_bytes} bytes")));
    }
    let mut file = File::open(path).map_err(|error| io_at("open asset metadata", path, error))?;
    let opened = file
        .metadata()
        .map_err(|error| io_at("inspect opened asset metadata", path, error))?;
    if !opened.is_file() || !same_file_identity(&before, &opened) {
        return Err(AssetManagerError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_at("read asset metadata", path, error))?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let after = file
        .metadata()
        .map_err(|error| io_at("reinspect asset metadata", path, error))?;
    if size > max_bytes || size != opened.len() || !same_file_snapshot(&opened, &after) {
        return Err(corrupt(path, "metadata changed or exceeded its size limit"));
    }
    serde_json::from_slice(&bytes).map_err(|source| AssetManagerError::Json {
        operation: "parse asset metadata",
        path: path.to_path_buf(),
        source,
    })
}

fn tree_is_symlink_free(path: &Path) -> AssetManagerResult<bool> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_at("inspect cleanup tree", path, error))?;
    if metadata.file_type().is_symlink() {
        return Ok(false);
    }
    if metadata.is_file() {
        return Ok(true);
    }
    if !metadata.is_dir() {
        return Ok(false);
    }
    for entry in read_directory(path, "scan cleanup tree")? {
        let entry = entry.map_err(|error| io_at("read cleanup tree entry", path, error))?;
        if !tree_is_symlink_free(&entry.path())? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn directory_has_only(path: &Path, allowed: &[&str]) -> AssetManagerResult<bool> {
    for entry in read_directory(path, "inspect managed asset directory contents")? {
        let entry =
            entry.map_err(|error| io_at("read managed asset directory entry", path, error))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            return Ok(false);
        };
        if !allowed.contains(&name.as_str()) {
            return Ok(false);
        }
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| io_at("inspect managed asset directory entry", &entry_path, error))?;
        if file_type.is_symlink() || !file_type.is_dir() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_file_snapshot(before: &Metadata, after: &Metadata) -> bool {
    same_file_identity(before, after)
        && before.len() == after.len()
        && match (before.modified(), after.modified()) {
            (Ok(before), Ok(after)) => before == after,
            _ => true,
        }
}

#[cfg(unix)]
fn same_file_identity(before: &Metadata, after: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == after.dev() && before.ino() == after.ino()
}

#[cfg(not(unix))]
fn same_file_identity(before: &Metadata, after: &Metadata) -> bool {
    before.len() == after.len()
        && match (before.created(), after.created()) {
            (Ok(before), Ok(after)) => before == after,
            _ => true,
        }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> AssetManagerResult<()> {
    let directory =
        File::open(path).map_err(|error| io_at("open directory for sync", path, error))?;
    directory
        .sync_all()
        .map_err(|error| io_at("sync managed asset directory", path, error))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> AssetManagerResult<()> {
    Ok(())
}

fn corrupt(path: &Path, message: impl Into<String>) -> AssetManagerError {
    AssetManagerError::CorruptManifest {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn io_at(operation: &'static str, path: impl AsRef<Path>, source: io::Error) -> AssetManagerError {
    AssetManagerError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-asset-manager-test-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn manager(test_dir: &TestDir, quota: u64) -> AssetManager {
        AssetManager::new(test_dir.path.join("asset-store"), quota).expect("create manager")
    }

    fn request(asset_id: &str, version: &str, bytes: &[u8]) -> AssetInstallRequest {
        AssetInstallRequest {
            asset_id: asset_id.to_string(),
            kind: AssetKind::Sidecar,
            version: version.to_string(),
            source: AssetSource {
                uri: format!("https://assets.example/{asset_id}/{version}"),
                revision: Some(format!("release-{version}")),
            },
            provenance: AssetProvenance {
                publisher: Some("Little Monkey tests".to_string()),
                retrieved_at_ms: Some(1_725_000_000_000),
                notes: Some("signed release metadata verified upstream".to_string()),
            },
            license: AssetLicense {
                name: "Apache-2.0".to_string(),
                spdx_id: Some("Apache-2.0".to_string()),
                url: Some("https://www.apache.org/licenses/LICENSE-2.0".to_string()),
                text: None,
            },
            platform: AssetPlatform::current(),
            expected_sha256: sha256_hex(bytes),
            size_bytes: u64::try_from(bytes.len()).expect("test payload length fits u64"),
        }
    }

    fn current_state(manager: &AssetManager, asset_id: &str) -> AssetStateManifest {
        let asset_dir = manager.asset_dir(asset_id).expect("valid test asset id");
        manager
            .load_state(asset_id, &asset_dir)
            .expect("load active state")
    }

    #[test]
    fn versioned_manifest_roundtrips_and_activation_rejects_tampering() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let bytes = b"trusted-sidecar-v1";
        let mut wrong = request("browser", "v1", bytes);
        wrong.expected_sha256 = "0".repeat(SHA256_HEX_LENGTH);

        let error = manager
            .install_reader(&wrong, Cursor::new(bytes))
            .expect_err("wrong checksum must fail");
        assert!(matches!(error, AssetManagerError::ChecksumMismatch { .. }));
        assert!(manager.status("browser").expect("status query").is_none());

        let installed = manager
            .install_reader(&request("browser", "v1", bytes), Cursor::new(bytes))
            .expect("install verified payload");
        assert_eq!(installed.manifest.manifest_version, ASSET_MANIFEST_VERSION);
        assert_eq!(installed.manifest.payload_file, PAYLOAD_FILE);
        assert_eq!(
            installed.manifest.source.revision.as_deref(),
            Some("release-v1")
        );
        assert_eq!(
            installed.manifest.license.spdx_id.as_deref(),
            Some("Apache-2.0")
        );

        let encoded = serde_json::to_vec(&installed.manifest).expect("serialize manifest");
        let decoded: AssetVersionManifest =
            serde_json::from_slice(&encoded).expect("deserialize manifest");
        assert_eq!(decoded, installed.manifest);

        let activated = manager.activate("browser", "v1").expect("activate v1");
        let state_before = activated.state;
        fs::write(&installed.payload_path, b"tampered-sidecar!").expect("tamper payload");

        let error = manager
            .activate("browser", "v1")
            .expect_err("every activation must verify checksum");
        assert!(matches!(
            error,
            AssetManagerError::ChecksumMismatch { .. } | AssetManagerError::SizeMismatch { .. }
        ));
        assert_eq!(current_state(&manager, "browser"), state_before);

        let status = manager
            .status("browser")
            .expect("status query")
            .expect("installed asset status");
        assert!(matches!(
            status.versions[0].integrity,
            AssetIntegrityStatus::Invalid { .. }
        ));
    }

    #[test]
    fn upgrade_preserves_last_known_good_and_rollback_is_checksum_verified() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let v1 = b"stable-v1";
        let v2 = b"candidate-v2";

        let first = manager
            .upgrade_reader(&request("runner", "v1", v1), Cursor::new(v1))
            .expect("install and activate v1");
        assert_eq!(first.state.active_version.as_deref(), Some("v1"));
        assert_eq!(first.state.last_known_good_version.as_deref(), Some("v1"));

        let second = manager
            .upgrade_reader(&request("runner", "v2", v2), Cursor::new(v2))
            .expect("install and activate v2");
        assert_eq!(second.state.active_version.as_deref(), Some("v2"));
        assert_eq!(second.state.last_known_good_version.as_deref(), Some("v1"));

        let v1_payload = manager
            .version_dir("runner", "v1")
            .expect("v1 path")
            .join(PAYLOAD_FILE);
        fs::write(&v1_payload, b"broken-v1").expect("tamper rollback target");
        let error = manager
            .rollback("runner")
            .expect_err("rollback must checksum its target");
        assert!(matches!(error, AssetManagerError::ChecksumMismatch { .. }));
        assert_eq!(current_state(&manager, "runner"), second.state);
        fs::write(&v1_payload, v1).expect("restore verified rollback target");

        let rollback = manager.rollback("runner").expect("rollback to verified v1");
        assert_eq!(rollback.state.active_version.as_deref(), Some("v1"));
        assert_eq!(rollback.version.manifest.version, "v1");
        assert_eq!(
            rollback.state.last_known_good_version.as_deref(),
            Some("v1")
        );
    }

    #[test]
    fn failed_checksum_and_platform_upgrades_never_change_active_state() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let v1 = b"known-good-v1";
        manager
            .upgrade_reader(&request("engine", "v1", v1), Cursor::new(v1))
            .expect("activate v1");
        let state_before = current_state(&manager, "engine");

        let expected_v2 = b"expected-v2";
        let error = manager
            .upgrade_reader(
                &request("engine", "v2", expected_v2),
                Cursor::new(b"corrupt!-v2"),
            )
            .expect_err("checksum failure must abort upgrade");
        assert!(matches!(error, AssetManagerError::ChecksumMismatch { .. }));
        assert_eq!(current_state(&manager, "engine"), state_before);
        assert_eq!(
            manager
                .status("engine")
                .expect("status query")
                .expect("engine status")
                .versions
                .len(),
            1
        );

        let v3 = b"other-platform-v3";
        let mut incompatible = request("engine", "v3", v3);
        incompatible.platform.os = "definitely-not-this-operating-system".to_string();
        manager
            .install_reader(&incompatible, Cursor::new(v3))
            .expect("store version metadata for another platform");
        let error = manager
            .activate("engine", "v3")
            .expect_err("wrong platform must not activate");
        assert!(matches!(
            error,
            AssetManagerError::IncompatiblePlatform { .. }
        ));
        assert_eq!(current_state(&manager, "engine"), state_before);
    }

    #[test]
    fn quota_is_enforced_before_staging_and_counts_actual_owned_payloads() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 10);
        let first = b"123456";
        let second = b"12345";
        manager
            .install_reader(&request("models", "v1", first), Cursor::new(first))
            .expect("install within quota");
        assert_eq!(manager.used_bytes().expect("calculate usage"), 6);

        let error = manager
            .install_reader(&request("models", "v2", second), Cursor::new(second))
            .expect_err("quota must reject second version");
        assert!(matches!(
            error,
            AssetManagerError::QuotaExceeded {
                quota: 10,
                used: 6,
                requested: 5
            }
        ));
        assert!(
            read_directory(&manager.temp_dir, "test temporary directory")
                .expect("list temp")
                .next()
                .is_none()
        );
        assert_eq!(
            manager
                .status("models")
                .expect("status query")
                .expect("models status")
                .versions
                .len(),
            1
        );
    }

    #[test]
    fn regular_file_install_is_verified_and_idempotent() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let bytes = b"regular-staged-sidecar";
        let source = test_dir.path.join("downloaded-sidecar.bin");
        fs::write(&source, bytes).expect("write staged regular file");
        let install_request = request("filesystem", "v1", bytes);

        let first = manager
            .install_file(&install_request, &source)
            .expect("install regular staged file");
        let second = manager
            .install_file(&install_request, &source)
            .expect("repeat install is idempotent");
        assert_eq!(first, second);

        let mut conflicting = install_request;
        conflicting.provenance.notes = Some("different provenance".to_string());
        let error = manager
            .install_file(&conflicting, &source)
            .expect_err("immutable version metadata cannot be replaced");
        assert!(matches!(error, AssetManagerError::VersionConflict { .. }));
    }

    #[test]
    fn identifiers_and_file_symlinks_cannot_escape_private_storage() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let bytes = b"sidecar";

        let error = manager
            .install_reader(&request("../escape", "v1", bytes), Cursor::new(bytes))
            .expect_err("path-like asset id must fail");
        assert!(matches!(
            error,
            AssetManagerError::InvalidIdentifier {
                field: "asset_id",
                ..
            }
        ));
        let error = manager
            .install_reader(&request("safe", "../escape", bytes), Cursor::new(bytes))
            .expect_err("path-like version must fail");
        assert!(matches!(
            error,
            AssetManagerError::InvalidIdentifier {
                field: "version",
                ..
            }
        ));
        assert!(!test_dir.path.join("escape").exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let source = test_dir.path.join("real-source.bin");
            let source_link = test_dir.path.join("source-link.bin");
            fs::write(&source, bytes).expect("write source");
            symlink(&source, &source_link).expect("create source symlink");
            let error = manager
                .install_file(&request("safe", "v1", bytes), &source_link)
                .expect_err("source symlink must fail");
            assert!(matches!(error, AssetManagerError::UnsafeFileType { .. }));

            let outside = test_dir.path.join("outside.bin");
            fs::write(&outside, bytes).expect("write outside file");
            let malicious_version = manager
                .asset_dir("safe")
                .expect("asset path")
                .join(VERSIONS_DIRECTORY)
                .join("v2");
            fs::create_dir_all(malicious_version.parent().expect("version parent"))
                .expect("create version parent");
            symlink(&outside, &malicious_version).expect("create internal symlink");
            let error = manager
                .install_reader(&request("safe", "v2", bytes), Cursor::new(bytes))
                .expect_err("internal symlink must fail");
            assert!(matches!(error, AssetManagerError::UnsafeFileType { .. }));
            assert_eq!(fs::read(&outside).expect("outside remains readable"), bytes);
        }
    }

    #[test]
    fn cleanup_removes_owned_orphans_but_never_follows_unsafe_entries() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let v1 = b"v1";
        let v2 = b"v2";
        manager
            .upgrade_reader(&request("worker", "v1", v1), Cursor::new(v1))
            .expect("activate v1");
        manager
            .upgrade_reader(&request("worker", "v2", v2), Cursor::new(v2))
            .expect("activate v2");
        manager
            .rollback("worker")
            .expect("create third state record");
        let state_before = current_state(&manager, "worker");

        let orphan = manager
            .temp_dir
            .join(format!("stage-{}", Uuid::new_v4().simple()));
        fs::create_dir(&orphan).expect("create orphan stage");
        fs::write(orphan.join("partial.bin"), b"partial").expect("write orphan payload");
        let empty_asset = manager.assets_dir.join("empty");
        fs::create_dir(&empty_asset).expect("create empty asset directory");

        #[cfg(unix)]
        let unsafe_temp = {
            use std::os::unix::fs::symlink;

            let outside = test_dir.path.join("cleanup-outside");
            fs::create_dir(&outside).expect("create outside directory");
            fs::write(outside.join("keep.txt"), b"keep").expect("write outside sentinel");
            let link = manager
                .temp_dir
                .join(format!("stage-{}", Uuid::new_v4().simple()));
            symlink(&outside, &link).expect("create unsafe temporary symlink");
            (link, outside)
        };

        let report = manager.cleanup_orphans().expect("cleanup managed orphans");
        assert_eq!(report.removed_temp_entries, 1);
        assert!(report.removed_state_records >= 2);
        assert_eq!(report.removed_empty_asset_directories, 1);
        assert!(!orphan.exists());
        assert!(!empty_asset.exists());
        assert_eq!(current_state(&manager, "worker"), state_before);

        #[cfg(unix)]
        {
            assert!(report.skipped_unsafe_entries.contains(&unsafe_temp.0));
            assert!(unsafe_temp.0.exists());
            assert_eq!(
                fs::read(unsafe_temp.1.join("keep.txt")).expect("outside sentinel remains"),
                b"keep"
            );
        }
    }

    #[test]
    fn reopen_preserves_active_pointer_metadata_and_rollback_history() {
        let test_dir = TestDir::new();
        let root = test_dir.path.join("persistent-store");
        let v1 = b"persistent-v1";
        let v2 = b"persistent-v2";
        {
            let manager = AssetManager::new(&root, 1024).expect("create first manager");
            manager
                .upgrade_reader(&request("agent", "v1", v1), Cursor::new(v1))
                .expect("activate v1");
            manager
                .upgrade_reader(&request("agent", "v2", v2), Cursor::new(v2))
                .expect("activate v2");
        }

        let reopened = AssetManager::new(&root, 1024).expect("reopen manager");
        let status = reopened
            .status("agent")
            .expect("status after reopen")
            .expect("agent is installed");
        assert_eq!(status.active_version.as_deref(), Some("v2"));
        assert_eq!(status.last_known_good_version.as_deref(), Some("v1"));
        assert_eq!(
            status
                .versions
                .iter()
                .map(|version| version.version.as_str())
                .collect::<Vec<_>>(),
            vec!["v1", "v2"]
        );
        assert_eq!(
            status.versions[1]
                .manifest
                .as_ref()
                .expect("verified manifest")
                .provenance
                .publisher
                .as_deref(),
            Some("Little Monkey tests")
        );
        assert_eq!(reopened.list().expect("list after reopen").len(), 1);

        let rollback = reopened.rollback("agent").expect("rollback after reopen");
        assert_eq!(rollback.state.active_version.as_deref(), Some("v1"));
    }

    #[test]
    fn removal_requires_owned_inactive_layout_and_archive_install_is_explicitly_unsupported() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let v1 = b"one";
        let v2 = b"two";
        let v3 = b"three";
        manager
            .upgrade_reader(&request("tools", "v1", v1), Cursor::new(v1))
            .expect("activate v1");
        let error = manager
            .remove_version("tools", "v1")
            .expect_err("active version must not be removed");
        assert!(matches!(error, AssetManagerError::VersionInUse { .. }));

        manager
            .install_reader(&request("tools", "v3", v3), Cursor::new(v3))
            .expect("install removable inactive version");
        assert!(manager
            .remove_version("tools", "v3")
            .expect("remove owned inactive version"));
        assert!(!manager
            .remove_version("tools", "v3")
            .expect("second removal is idempotent"));

        manager
            .install_reader(&request("tools", "v2", v2), Cursor::new(v2))
            .expect("install v2");
        fs::remove_file(
            manager
                .version_dir("tools", "v2")
                .expect("version path")
                .join(OWNERSHIP_FILE),
        )
        .expect("remove ownership marker");
        let error = manager
            .remove_version("tools", "v2")
            .expect_err("unowned layout must not be removed");
        assert!(matches!(error, AssetManagerError::UnownedVersion { .. }));

        let nonexistent_archive = test_dir.path.join("never-opened.tar");
        let error = manager
            .install_archive_file(
                &request("tools", "archive", b"archive"),
                &nonexistent_archive,
            )
            .expect_err("archive extraction must be explicitly unsupported");
        assert!(matches!(
            error,
            AssetManagerError::ArchiveExtractionUnsupported
        ));
        assert!(!nonexistent_archive.exists());
    }

    #[test]
    fn marking_active_good_advances_the_removal_boundary() {
        let test_dir = TestDir::new();
        let manager = manager(&test_dir, 1024);
        let v1 = b"first";
        let v2 = b"second";
        manager
            .upgrade_reader(&request("service", "v1", v1), Cursor::new(v1))
            .expect("activate v1");
        manager
            .upgrade_reader(&request("service", "v2", v2), Cursor::new(v2))
            .expect("activate v2");
        assert!(matches!(
            manager.remove_version("service", "v1"),
            Err(AssetManagerError::VersionInUse { .. })
        ));

        let promoted = manager
            .mark_active_last_known_good("service")
            .expect("promote healthy v2");
        assert_eq!(
            promoted.state.last_known_good_version.as_deref(),
            Some("v2")
        );
        assert!(manager
            .remove_version("service", "v1")
            .expect("old version is now removable"));
        assert_eq!(manager.used_bytes().expect("usage after removal"), 6);
    }
}
