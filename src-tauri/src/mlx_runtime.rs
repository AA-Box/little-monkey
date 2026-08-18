//! Apple-Silicon MLX runtime core.
//!
//! The module is Tauri-free and makes every privileged boundary explicit:
//! host probing, package signature verification, and service process control
//! are injected. The package installer itself performs bounded manifest,
//! digest, path, atomic-publication, and activation verification inside an
//! app-private root. No user Python environment or shell command is used.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const MLX_RUNTIME_SCHEMA_VERSION: u32 = 1;
pub const MLX_PACKAGE_SCHEMA_VERSION: u32 = 1;
pub const MLX_ACTIVE_STATE_SCHEMA_VERSION: u32 = 1;
pub const MAX_MLX_UNLOAD_WAIT_MS: u64 = 10_000;

const VERSIONS_DIRECTORY: &str = "versions";
const STAGING_DIRECTORY: &str = ".staging";
const ACTIVE_STATE_FILE: &str = "active.json";
const INSTALL_MANIFEST_FILE: &str = ".mlx-package.json";
const MAX_ID_BYTES: usize = 256;
const MAX_PATH_BYTES: usize = 4_096;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

pub type MlxResult<T> = Result<T, MlxError>;
pub type MlxFuture<'a, T> = Pin<Box<dyn Future<Output = MlxResult<T>> + Send + 'a>>;

#[derive(Debug)]
pub enum MlxError {
    Unavailable {
        reason: MlxUnavailableReason,
        message: String,
    },
    Invalid {
        field: String,
        message: String,
    },
    Limit {
        name: &'static str,
        observed: u64,
        max: u64,
    },
    Signature(String),
    DigestMismatch {
        path: String,
        expected: String,
        actual: String,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    Controller {
        operation: String,
        message: String,
    },
    Cancelled {
        operation: String,
    },
    Timeout {
        operation: String,
        timeout_ms: u64,
    },
    PortBusy {
        port: u16,
        owner: String,
    },
    NotInstalled,
    NotRunning,
    ModelNotFound(String),
    ModelAlreadyRunning(String),
    RequestAlreadyRunning(String),
    StreamProtocol(String),
    LockPoisoned,
}

impl fmt::Display for MlxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable { reason, message } => {
                write!(f, "MLX is unavailable ({reason:?}): {message}")
            }
            Self::Invalid { field, message } => write!(f, "invalid {field}: {message}"),
            Self::Limit {
                name,
                observed,
                max,
            } => write!(f, "{name} is {observed}, exceeding {max}"),
            Self::Signature(message) => write!(f, "MLX package signature failed: {message}"),
            Self::DigestMismatch {
                path,
                expected,
                actual,
            } => write!(
                f,
                "MLX package digest mismatch for {path} (expected {expected}, got {actual})"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Json(error) => write!(f, "MLX JSON error: {error}"),
            Self::Controller { operation, message } => {
                write!(f, "MLX controller failed during {operation}: {message}")
            }
            Self::Cancelled { operation } => write!(f, "MLX {operation} was cancelled"),
            Self::Timeout {
                operation,
                timeout_ms,
            } => write!(f, "MLX {operation} timed out after {timeout_ms} ms"),
            Self::PortBusy { port, owner } => {
                write!(f, "MLX port {port} is already owned by {owner}")
            }
            Self::NotInstalled => write!(f, "the verified MLX runtime is not installed"),
            Self::NotRunning => write!(f, "the managed MLX service is not running"),
            Self::ModelNotFound(model) => write!(f, "MLX model {model:?} is not registered"),
            Self::ModelAlreadyRunning(model) => {
                write!(f, "MLX is already serving model {model:?}")
            }
            Self::RequestAlreadyRunning(request) => {
                write!(f, "MLX request {request:?} is already running")
            }
            Self::StreamProtocol(message) => write!(f, "invalid MLX stream: {message}"),
            Self::LockPoisoned => write!(f, "MLX adapter state lock is poisoned"),
        }
    }
}

impl Error for MlxError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for MlxError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MlxUnavailableReason {
    NonMacOs,
    NonAppleSilicon,
    MetalUnavailable,
    ProbeFailed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxHostCapabilities {
    pub os: String,
    pub architecture: String,
    pub apple_silicon: bool,
    pub metal_available: bool,
    pub unified_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
    pub unavailable_reason: Option<MlxUnavailableReason>,
}

impl MlxHostCapabilities {
    pub fn evaluate(
        os: &str,
        architecture: &str,
        metal_available: bool,
        unified_memory_bytes: Option<u64>,
        available_memory_bytes: Option<u64>,
    ) -> Self {
        let os = normalize_os(os);
        let architecture = normalize_arch(architecture);
        let apple_silicon = os == "macos" && architecture == "aarch64";
        let unavailable_reason = if os != "macos" {
            Some(MlxUnavailableReason::NonMacOs)
        } else if architecture != "aarch64" {
            Some(MlxUnavailableReason::NonAppleSilicon)
        } else if !metal_available {
            Some(MlxUnavailableReason::MetalUnavailable)
        } else {
            None
        };
        Self {
            os,
            architecture,
            apple_silicon,
            metal_available: apple_silicon && metal_available,
            unified_memory_bytes,
            available_memory_bytes,
            unavailable_reason,
        }
    }

    pub fn current() -> Self {
        let supported_target = cfg!(all(target_os = "macos", target_arch = "aarch64"));
        Self::evaluate(
            std::env::consts::OS,
            std::env::consts::ARCH,
            supported_target,
            None,
            None,
        )
    }

    pub fn is_available(&self) -> bool {
        self.unavailable_reason.is_none()
            && self.apple_silicon
            && self.metal_available
            && self.os == "macos"
            && self.architecture == "aarch64"
    }

    pub fn ensure_available(&self) -> MlxResult<()> {
        if self.is_available() {
            Ok(())
        } else {
            Err(MlxError::Unavailable {
                reason: self
                    .unavailable_reason
                    .unwrap_or(MlxUnavailableReason::ProbeFailed),
                message: "MLX requires a Metal-capable Apple Silicon Mac".to_string(),
            })
        }
    }
}

pub trait MlxCapabilityProbe: Send + Sync {
    fn probe(&self) -> MlxResult<MlxHostCapabilities>;
}

#[derive(Default)]
pub struct CurrentHostMlxProbe;

impl MlxCapabilityProbe for CurrentHostMlxProbe {
    fn probe(&self) -> MlxResult<MlxHostCapabilities> {
        Ok(MlxHostCapabilities::current())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxPackageFile {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxPackageManifest {
    pub schema_version: u32,
    pub package_version: String,
    pub target_os: String,
    pub target_architecture: String,
    pub python_executable: String,
    pub service_entry: String,
    pub files: Vec<MlxPackageFile>,
    pub signature_algorithm: String,
    pub signature_key_id: String,
    pub signature_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlxPackageBundle {
    pub manifest: MlxPackageManifest,
    pub files: BTreeMap<String, Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxInstallLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_manifest_bytes: usize,
}

impl Default for MlxInstallLimits {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_file_bytes: 2 * 1024 * 1024 * 1024,
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            // Must be able to hold `max_files` entries, or the two limits
            // contradict each other and the smaller one silently wins. Each
            // entry is a path plus a 64-character digest — around 200 bytes —
            // so 20,000 of them need roughly 4 MiB. At the previous 2 MiB a
            // real package (a Python runtime is thousands of files) was
            // rejected for its manifest length rather than anything about it.
            max_manifest_bytes: 8 * 1024 * 1024,
        }
    }
}

pub trait MlxSignatureVerifier: Send + Sync {
    /// Verify `signature` over the exact canonical `signed_payload` bytes.
    /// Production implementations must use a pinned publisher key and an
    /// audited signature algorithm; returning success is authorization to
    /// publish executable code into the private runtime directory.
    fn verify(
        &self,
        algorithm: &str,
        key_id: &str,
        signed_payload: &[u8],
        signature: &[u8],
    ) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UnsignedMlxPackageManifest {
    schema_version: u32,
    package_version: String,
    target_os: String,
    target_architecture: String,
    python_executable: String,
    service_entry: String,
    files: Vec<MlxPackageFile>,
    signature_algorithm: String,
    signature_key_id: String,
}

impl From<&MlxPackageManifest> for UnsignedMlxPackageManifest {
    fn from(manifest: &MlxPackageManifest) -> Self {
        Self {
            schema_version: manifest.schema_version,
            package_version: manifest.package_version.clone(),
            target_os: manifest.target_os.clone(),
            target_architecture: manifest.target_architecture.clone(),
            python_executable: manifest.python_executable.clone(),
            service_entry: manifest.service_entry.clone(),
            files: manifest.files.clone(),
            signature_algorithm: manifest.signature_algorithm.clone(),
            signature_key_id: manifest.signature_key_id.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MlxActiveState {
    schema_version: u32,
    package_version: String,
    manifest_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedMlxInstall {
    pub package_version: String,
    pub version_directory: PathBuf,
    pub python_executable: PathBuf,
    pub service_entry: PathBuf,
    pub manifest_sha256: String,
}

pub struct MlxPackageInstaller {
    root: PathBuf,
    verifier: Arc<dyn MlxSignatureVerifier>,
    limits: MlxInstallLimits,
    operation_lock: Mutex<()>,
}

/// The manifest a built, not-yet-installed package carries at its root.
///
/// Deliberately not [`INSTALL_MANIFEST_FILE`]: that name marks a tree this
/// installer has already published and verified, and reusing it would make a
/// downloaded directory indistinguishable from an installed one.
pub const MLX_SOURCE_MANIFEST_FILE: &str = "mlx-package.json";

/// Unpacks a `.tar.gz` MLX package into `destination`.
///
/// This is the seam between the component hub, which downloads one opaque
/// blob and proves only its SHA-256, and [`MlxPackageInstaller`], which wants
/// a tree and proves the publisher signed it. The two checks layer: the digest
/// says the bytes are the ones the catalog listed, the signature says the
/// publisher made them. Neither substitutes for the other.
///
/// Every archive entry is treated as hostile data, because an archive is the
/// classic path-traversal sink:
///
///   * the path is re-validated as a normal relative path, so `../` and
///     absolute entries never reach a `join`
///   * the joined result is confirmed to stay under `destination`, which
///     catches anything the first check did not anticipate
///   * only regular files are written — a symlink entry in an archive is how a
///     later entry gets redirected outside the tree, and honoring one here
///     would undo both checks above
///   * entry count and byte totals are bounded before anything is written
///
/// Modes come from the manifest, never from the archive: the installer sets
/// 0o700 or 0o600 per its own `executable` flag, so a tampered header cannot
/// make a data file executable.
pub fn extract_package_archive(
    archive: &Path,
    destination: &Path,
    limits: &MlxInstallLimits,
) -> MlxResult<()> {
    let file =
        File::open(archive).map_err(|source| io_at("open MLX package archive", archive, source))?;
    let mut reader = tar::Archive::new(flate2::read::GzDecoder::new(file));
    ensure_private_directory(destination)?;

    let mut count = 0_usize;
    let mut total = 0_u64;
    let entries = reader
        .entries()
        .map_err(|source| io_at("read MLX package archive", archive, source))?;
    for entry in entries {
        let mut entry = entry.map_err(|source| io_at("read MLX package entry", archive, source))?;
        // Directories are implied by the files inside them; anything that is
        // neither a directory nor a regular file has no place in a package.
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(invalid(
                "archive.entry",
                "package archives may contain only regular files",
            ));
        }
        count += 1;
        if count > limits.max_files {
            return Err(limit(
                "MLX package files",
                count as u64,
                limits.max_files as u64,
            ));
        }
        let size = entry.header().size().unwrap_or(u64::MAX);
        if size > limits.max_file_bytes {
            return Err(limit("MLX package file bytes", size, limits.max_file_bytes));
        }
        total = total.saturating_add(size);
        if total > limits.max_total_bytes {
            return Err(limit("MLX package bytes", total, limits.max_total_bytes));
        }

        let raw = entry
            .path()
            .map_err(|_| invalid("archive.entry.path", "is not a usable path"))?
            .to_string_lossy()
            .to_string();
        validate_relative_path(&raw, "archive.entry.path")?;
        let path = destination.join(&raw);
        // Belt and braces: `validate_relative_path` already rejects traversal,
        // but this is the check that is true by construction rather than by
        // reasoning about every path rule.
        if !path.starts_with(destination) {
            return Err(invalid("archive.entry.path", "escapes the package root"));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|source| io_at("create package directory", parent, source))?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options
            .open(&path)
            .map_err(|source| io_at("create package file", &path, source))?;
        io::copy(&mut entry, &mut output)
            .map_err(|source| io_at("write package file", &path, source))?;
    }
    Ok(())
}

/// Reads a built package directory into the bundle [`MlxPackageInstaller`]
/// installs.
///
/// The installer takes its input in memory and verifies the signature over it,
/// so this reads rather than trusts: only the files the manifest declares are
/// loaded, each is bounded by the same limits the install will re-check, and
/// nothing outside the directory is reachable because every declared path has
/// already been validated as a normal relative path. A file present on disk
/// but absent from the manifest is not loaded and therefore cannot be
/// installed — the signature covers the list, not the directory.
pub fn read_package_directory(
    directory: &Path,
    limits: &MlxInstallLimits,
) -> MlxResult<MlxPackageBundle> {
    let manifest_path = directory.join(MLX_SOURCE_MANIFEST_FILE);
    let manifest_bytes = read_regular_bounded(&manifest_path, limits.max_manifest_bytes)?;
    let manifest: MlxPackageManifest = serde_json::from_slice(&manifest_bytes)?;
    // Shape only. Whether this package matches *this* host is the installer's
    // question, asked against a probed `MlxHostCapabilities` rather than
    // against whatever the caller happens to be reading the directory on.
    validate_manifest_target_shape(&manifest, limits)?;

    let mut files = BTreeMap::new();
    let mut total: u64 = 0;
    for file in &manifest.files {
        // Re-validated here even though the manifest shape check passed: this
        // is the value about to be joined onto a real path on this machine.
        validate_relative_path(&file.path, "files.path")?;
        let bytes = read_regular_bounded(
            &directory.join(&file.path),
            usize::try_from(limits.max_file_bytes).unwrap_or(usize::MAX),
        )?;
        total = total.saturating_add(bytes.len() as u64);
        if total > limits.max_total_bytes {
            return Err(limit("MLX package bytes", total, limits.max_total_bytes));
        }
        files.insert(file.path.clone(), bytes);
    }
    Ok(MlxPackageBundle { manifest, files })
}

impl MlxPackageInstaller {
    pub fn new(
        root: impl AsRef<Path>,
        verifier: Arc<dyn MlxSignatureVerifier>,
        limits: MlxInstallLimits,
    ) -> MlxResult<Self> {
        validate_install_limits(&limits)?;
        Ok(Self {
            root: root.as_ref().to_path_buf(),
            verifier,
            limits,
            operation_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn install_and_activate(
        &self,
        bundle: &MlxPackageBundle,
        host: &MlxHostCapabilities,
    ) -> MlxResult<VerifiedMlxInstall> {
        let _guard = lock(&self.operation_lock)?;
        host.ensure_available()?;
        let prepared = self.preflight(bundle, host)?;
        ensure_private_directory(&self.root)?;
        let versions = self.root.join(VERSIONS_DIRECTORY);
        let staging_root = self.root.join(STAGING_DIRECTORY);
        ensure_private_directory(&versions)?;
        ensure_private_directory(&staging_root)?;
        let destination = versions.join(&prepared.version_directory_name);
        match fs::symlink_metadata(&destination) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                let verified = self.verify_version_directory(&destination)?;
                if verified.manifest_sha256 != prepared.manifest_sha256 {
                    return Err(invalid(
                        "packageVersion",
                        "an installed version exists with a different manifest",
                    ));
                }
                self.activate(&verified)?;
                return Ok(verified);
            }
            Ok(_) => {
                return Err(invalid(
                    "packageVersion",
                    "install destination is not a real directory",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(io_at("inspect install destination", &destination, source)),
        }

        let staging = staging_root.join(format!("install-{}", Uuid::new_v4()));
        fs::create_dir(&staging)
            .map_err(|source| io_at("create install staging", &staging, source))?;
        #[cfg(unix)]
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_at("secure install staging", &staging, source))?;
        let write_result = (|| -> MlxResult<()> {
            for file in &bundle.manifest.files {
                let bytes = bundle
                    .files
                    .get(&file.path)
                    .ok_or_else(|| invalid(&file.path, "package payload disappeared"))?;
                let path = staging.join(&file.path);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|source| io_at("create package directory", parent, source))?;
                    harden_directory_tree(parent, &staging)?;
                }
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                options.mode(if file.executable { 0o700 } else { 0o600 });
                let mut output = options
                    .open(&path)
                    .map_err(|source| io_at("create package file", &path, source))?;
                output
                    .write_all(bytes)
                    .and_then(|_| output.sync_all())
                    .map_err(|source| io_at("write package file", &path, source))?;
            }
            let manifest_path = staging.join(INSTALL_MANIFEST_FILE);
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut output = options
                .open(&manifest_path)
                .map_err(|source| io_at("create package manifest", &manifest_path, source))?;
            output
                .write_all(&prepared.manifest_json)
                .and_then(|_| output.sync_all())
                .map_err(|source| io_at("write package manifest", &manifest_path, source))?;
            sync_directory(&staging)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        let staged_verified = self.verify_version_directory(&staging)?;
        if staged_verified.manifest_sha256 != prepared.manifest_sha256 {
            let _ = fs::remove_dir_all(&staging);
            return Err(invalid(
                "manifest",
                "staged verification changed the manifest",
            ));
        }
        if let Err(source) = fs::rename(&staging, &destination) {
            let _ = fs::remove_dir_all(&staging);
            return Err(io_at("publish MLX package", &destination, source));
        }
        sync_directory(&versions)?;
        let verified = self.verify_version_directory(&destination)?;
        self.activate(&verified)?;
        Ok(verified)
    }

    pub fn verify_active(&self) -> MlxResult<VerifiedMlxInstall> {
        let _guard = lock(&self.operation_lock)?;
        let active_path = self.root.join(ACTIVE_STATE_FILE);
        let bytes = match read_regular_bounded(&active_path, self.limits.max_manifest_bytes) {
            Ok(bytes) => bytes,
            Err(MlxError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Err(MlxError::NotInstalled)
            }
            Err(error) => return Err(error),
        };
        let active: MlxActiveState = serde_json::from_slice(&bytes)?;
        if active.schema_version != MLX_ACTIVE_STATE_SCHEMA_VERSION {
            return Err(invalid("active.schemaVersion", "is unsupported"));
        }
        validate_id(&active.package_version, "active.packageVersion")?;
        validate_sha256(&active.manifest_sha256, "active.manifestSha256")?;
        let destination = self
            .root
            .join(VERSIONS_DIRECTORY)
            .join(version_directory_name(
                &active.package_version,
                &active.manifest_sha256,
            ));
        let verified = self.verify_version_directory(&destination)?;
        if verified.manifest_sha256 != active.manifest_sha256 {
            return Err(invalid(
                "active.manifestSha256",
                "does not match the verified package",
            ));
        }
        Ok(verified)
    }

    fn preflight(
        &self,
        bundle: &MlxPackageBundle,
        host: &MlxHostCapabilities,
    ) -> MlxResult<PreparedPackage> {
        validate_manifest(&bundle.manifest, &self.limits, host)?;
        let declared = bundle
            .manifest
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<BTreeSet<_>>();
        let supplied = bundle
            .files
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if declared != supplied {
            return Err(invalid(
                "files",
                "supplied payload names must exactly match the signed manifest",
            ));
        }
        for file in &bundle.manifest.files {
            let bytes = bundle.files.get(&file.path).expect("sets matched");
            let size = bytes.len() as u64;
            if size != file.size_bytes {
                return Err(invalid(
                    &file.path,
                    format!("declared size {} differs from {size}", file.size_bytes),
                ));
            }
            let actual = sha256_hex(bytes);
            if actual != file.sha256 {
                return Err(MlxError::DigestMismatch {
                    path: file.path.clone(),
                    expected: file.sha256.clone(),
                    actual,
                });
            }
        }
        let signed_payload = canonical_json(&UnsignedMlxPackageManifest::from(&bundle.manifest))?;
        let signature = decode_signature(&bundle.manifest.signature_base64)?;
        self.verifier
            .verify(
                &bundle.manifest.signature_algorithm,
                &bundle.manifest.signature_key_id,
                &signed_payload,
                &signature,
            )
            .map_err(MlxError::Signature)?;
        let manifest_json = canonical_json(&bundle.manifest)?;
        if manifest_json.len() > self.limits.max_manifest_bytes {
            return Err(limit(
                "MLX manifest bytes",
                manifest_json.len() as u64,
                self.limits.max_manifest_bytes as u64,
            ));
        }
        let manifest_sha256 = sha256_hex(&manifest_json);
        Ok(PreparedPackage {
            version_directory_name: version_directory_name(
                &bundle.manifest.package_version,
                &manifest_sha256,
            ),
            manifest_sha256,
            manifest_json,
        })
    }

    fn verify_version_directory(&self, directory: &Path) -> MlxResult<VerifiedMlxInstall> {
        let metadata = fs::symlink_metadata(directory)
            .map_err(|source| io_at("inspect MLX version", directory, source))?;
        if !metadata.file_type().is_dir() {
            return Err(invalid("install", "version path is not a real directory"));
        }
        let manifest_path = directory.join(INSTALL_MANIFEST_FILE);
        let manifest_json = read_regular_bounded(&manifest_path, self.limits.max_manifest_bytes)?;
        let manifest: MlxPackageManifest = serde_json::from_slice(&manifest_json)?;
        validate_manifest_target_shape(&manifest, &self.limits)?;
        let signature = decode_signature(&manifest.signature_base64)?;
        let signed_payload = canonical_json(&UnsignedMlxPackageManifest::from(&manifest))?;
        self.verifier
            .verify(
                &manifest.signature_algorithm,
                &manifest.signature_key_id,
                &signed_payload,
                &signature,
            )
            .map_err(MlxError::Signature)?;
        let mut expected_paths = BTreeSet::new();
        for file in &manifest.files {
            expected_paths.insert(file.path.clone());
            let path = directory.join(&file.path);
            let bytes = read_regular_bounded(
                &path,
                usize::try_from(file.size_bytes)
                    .unwrap_or(usize::MAX)
                    .min(usize::try_from(self.limits.max_file_bytes).unwrap_or(usize::MAX)),
            )?;
            if bytes.len() as u64 != file.size_bytes {
                return Err(invalid(&file.path, "installed file size changed"));
            }
            let actual = sha256_hex(&bytes);
            if actual != file.sha256 {
                return Err(MlxError::DigestMismatch {
                    path: file.path.clone(),
                    expected: file.sha256.clone(),
                    actual,
                });
            }
        }
        reject_unexpected_install_entries(directory, directory, &expected_paths)?;
        let manifest_sha256 = sha256_hex(&canonical_json(&manifest)?);
        Ok(VerifiedMlxInstall {
            package_version: manifest.package_version,
            python_executable: directory.join(manifest.python_executable),
            service_entry: directory.join(manifest.service_entry),
            version_directory: directory.to_path_buf(),
            manifest_sha256,
        })
    }

    fn activate(&self, install: &VerifiedMlxInstall) -> MlxResult<()> {
        let active = MlxActiveState {
            schema_version: MLX_ACTIVE_STATE_SCHEMA_VERSION,
            package_version: install.package_version.clone(),
            manifest_sha256: install.manifest_sha256.clone(),
        };
        let bytes = canonical_json(&active)?;
        atomic_write_private(&self.root.join(ACTIVE_STATE_FILE), &bytes)
    }
}

struct PreparedPackage {
    version_directory_name: String,
    manifest_sha256: String,
    manifest_json: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxModelCapabilities {
    pub chat: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub structured_output: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxModelRecord {
    pub model_id: String,
    pub display_name: String,
    pub local_path: PathBuf,
    pub size_bytes: u64,
    pub revision: Option<String>,
    pub capabilities: MlxModelCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxRuntimeConfig {
    pub runtime_id: String,
    pub port: u16,
    pub unload_timeout_ms: u64,
    pub max_log_bytes: usize,
    pub max_stream_bytes: usize,
}

impl Default for MlxRuntimeConfig {
    fn default() -> Self {
        Self {
            runtime_id: "mlx".to_string(),
            port: 8_081,
            unload_timeout_ms: MAX_MLX_UNLOAD_WAIT_MS,
            max_log_bytes: 512 * 1024,
            max_stream_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxProcessHandle {
    pub process_id: String,
    pub os_pid: Option<u32>,
    pub port: u16,
    pub model_id: String,
    pub started_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxProcessMetrics {
    pub process_alive: bool,
    pub resident_memory_bytes: u64,
    pub unified_memory_bytes: u64,
    pub active_requests: u64,
    pub generated_tokens: u64,
    pub tokens_per_second: Option<f64>,
    pub sampled_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MlxLaunchSpec {
    pub runtime_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub port: u16,
    pub model_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxMessage {
    pub role: String,
    pub text: String,
    /// Inline images attached to this turn, as `data:<mime>;base64,<data>`
    /// URIs (ROADMAP Phase 8 item 12). Empty for every non-vision message —
    /// the verified, separately-installed MLX service package is expected to
    /// decode and hand these to an MLX-VLM-style vision tower/projector; this
    /// struct only carries the bytes across the process boundary.
    #[serde(default)]
    pub images: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxGenerationRequest {
    pub request_id: String,
    pub model_id: String,
    pub messages: Vec<MlxMessage>,
    pub tools: Vec<MlxToolDefinition>,
    pub max_tokens: u32,
    pub temperature: Option<f64>,
    pub structured_output_schema: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MlxStreamEvent {
    Started {
        request_id: String,
    },
    TextDelta {
        text: String,
    },
    ToolCallStart {
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        call_id: String,
        json: String,
    },
    ToolCallEnd {
        call_id: String,
    },
    Completed {
        input_tokens: u64,
        output_tokens: u64,
    },
    Error {
        code: String,
        message: String,
    },
}

pub trait MlxStreamSink: Send {
    fn emit(&mut self, event: MlxStreamEvent) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MlxGenerationSummary {
    pub request_id: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub finish_reason: String,
}

#[derive(Clone, Debug)]
pub struct MlxOperationContext {
    pub cancellation: CancellationToken,
    pub timeout_ms: u64,
}

impl Default for MlxOperationContext {
    fn default() -> Self {
        Self {
            cancellation: CancellationToken::new(),
            timeout_ms: 120_000,
        }
    }
}

pub trait MlxServiceController: Send + Sync {
    fn port_owner<'a>(&'a self, port: u16) -> MlxFuture<'a, Option<String>>;
    fn launch<'a>(
        &'a self,
        spec: MlxLaunchSpec,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessHandle>;
    fn inspect<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics>;
    /// Controllers must stop generation and propagate any error returned by
    /// `sink.emit`; swallowing a sink error would bypass stream validation.
    fn stream<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        request: &'a MlxGenerationRequest,
        sink: &'a mut dyn MlxStreamSink,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxGenerationSummary>;
    fn cancel<'a>(&'a self, handle: &'a MlxProcessHandle, request_id: &'a str)
        -> MlxFuture<'a, ()>;
    fn terminate_and_wait<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        timeout_ms: u64,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, MlxProcessMetrics>;
    fn tail_logs<'a>(
        &'a self,
        handle: &'a MlxProcessHandle,
        max_bytes: usize,
        context: &'a MlxOperationContext,
    ) -> MlxFuture<'a, String>;
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MlxRuntimeStatus {
    Unavailable {
        capabilities: MlxHostCapabilities,
    },
    NotInstalled {
        capabilities: MlxHostCapabilities,
    },
    Stopped {
        capabilities: MlxHostCapabilities,
        package_version: String,
    },
    Running {
        capabilities: MlxHostCapabilities,
        package_version: String,
        handle: MlxProcessHandle,
        metrics: MlxProcessMetrics,
    },
}

#[derive(Default)]
struct MlxAdapterState {
    running: Option<MlxProcessHandle>,
    active_requests: BTreeSet<String>,
}

pub struct MlxRuntimeAdapter {
    config: MlxRuntimeConfig,
    probe: Arc<dyn MlxCapabilityProbe>,
    installer: Arc<MlxPackageInstaller>,
    controller: Arc<dyn MlxServiceController>,
    models: BTreeMap<String, MlxModelRecord>,
    state: Mutex<MlxAdapterState>,
}

impl MlxRuntimeAdapter {
    pub fn new(
        config: MlxRuntimeConfig,
        probe: Arc<dyn MlxCapabilityProbe>,
        installer: Arc<MlxPackageInstaller>,
        controller: Arc<dyn MlxServiceController>,
        models: Vec<MlxModelRecord>,
    ) -> MlxResult<Self> {
        validate_runtime_config(&config)?;
        let mut by_id = BTreeMap::new();
        for model in models {
            validate_model(&model)?;
            if by_id.insert(model.model_id.clone(), model).is_some() {
                return Err(invalid("models", "duplicate MLX model id"));
            }
        }
        Ok(Self {
            config,
            probe,
            installer,
            controller,
            models: by_id,
            state: Mutex::new(MlxAdapterState::default()),
        })
    }

    pub fn capabilities(&self) -> MlxResult<MlxHostCapabilities> {
        self.probe.probe()
    }

    pub fn models(&self) -> Vec<MlxModelRecord> {
        self.models.values().cloned().collect()
    }

    pub fn has_verified_install(&self) -> bool {
        self.installer.verify_active().is_ok()
    }

    pub async fn status(&self, context: &MlxOperationContext) -> MlxResult<MlxRuntimeStatus> {
        validate_context(context)?;
        let capabilities = self.probe.probe()?;
        if !capabilities.is_available() {
            return Ok(MlxRuntimeStatus::Unavailable { capabilities });
        }
        let install = match self.installer.verify_active() {
            Ok(install) => install,
            Err(MlxError::NotInstalled) => {
                return Ok(MlxRuntimeStatus::NotInstalled { capabilities })
            }
            Err(error) => return Err(error),
        };
        let running = lock(&self.state)?.running.clone();
        let Some(handle) = running else {
            return Ok(MlxRuntimeStatus::Stopped {
                capabilities,
                package_version: install.package_version,
            });
        };
        let metrics = run_bounded(
            context,
            "inspect",
            self.controller.inspect(&handle, context),
        )
        .await?;
        if !metrics.process_alive {
            let mut state = lock(&self.state)?;
            if state.running.as_ref() == Some(&handle) {
                state.running = None;
                state.active_requests.clear();
            }
            return Ok(MlxRuntimeStatus::Stopped {
                capabilities,
                package_version: install.package_version,
            });
        }
        Ok(MlxRuntimeStatus::Running {
            capabilities,
            package_version: install.package_version,
            handle,
            metrics,
        })
    }

    pub async fn start(
        &self,
        model_id: &str,
        context: &MlxOperationContext,
    ) -> MlxResult<MlxProcessHandle> {
        validate_context(context)?;
        let capabilities = self.probe.probe()?;
        capabilities.ensure_available()?;
        let install = self.installer.verify_active()?;
        let model = self
            .models
            .get(model_id)
            .ok_or_else(|| MlxError::ModelNotFound(model_id.to_string()))?;
        let existing = {
            let state = lock(&self.state)?;
            state.running.clone()
        };
        if let Some(running) = existing {
            let metrics = run_bounded(
                context,
                "inspect existing process",
                self.controller.inspect(&running, context),
            )
            .await?;
            if metrics.process_alive {
                return if running.model_id == model_id {
                    Ok(running)
                } else {
                    Err(MlxError::ModelAlreadyRunning(running.model_id))
                };
            }
            {
                let mut state = lock(&self.state)?;
                if state.running.as_ref() == Some(&running) {
                    state.running = None;
                    state.active_requests.clear();
                }
            }
        }
        if let Some(owner) = run_bounded(
            context,
            "inspect port",
            self.controller.port_owner(self.config.port),
        )
        .await?
        {
            return Err(MlxError::PortBusy {
                port: self.config.port,
                owner,
            });
        }
        let spec = MlxLaunchSpec {
            runtime_id: self.config.runtime_id.clone(),
            program: install.python_executable,
            args: vec![
                install.service_entry.to_string_lossy().to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                self.config.port.to_string(),
                "--model".to_string(),
                model.local_path.to_string_lossy().to_string(),
            ],
            port: self.config.port,
            model_id: model_id.to_string(),
        };
        validate_launch_spec(&spec)?;
        let handle = run_bounded(context, "start", self.controller.launch(spec, context)).await?;
        validate_handle(&handle, &self.config, model_id)?;
        let raced = {
            let mut state = lock(&self.state)?;
            if state.running.is_some() {
                true
            } else {
                state.running = Some(handle.clone());
                false
            }
        };
        if raced {
            let _ = self
                .controller
                .terminate_and_wait(&handle, self.config.unload_timeout_ms, context)
                .await;
            return Err(MlxError::ModelAlreadyRunning(model_id.to_string()));
        }
        Ok(handle)
    }

    pub async fn stream(
        &self,
        request: &MlxGenerationRequest,
        sink: &mut dyn MlxStreamSink,
        context: &MlxOperationContext,
    ) -> MlxResult<MlxGenerationSummary> {
        validate_context(context)?;
        self.probe.probe()?.ensure_available()?;
        let model = self
            .models
            .get(&request.model_id)
            .ok_or_else(|| MlxError::ModelNotFound(request.model_id.clone()))?;
        validate_generation_request(request, model)?;
        let handle = {
            let mut state = lock(&self.state)?;
            let handle = state.running.clone().ok_or(MlxError::NotRunning)?;
            if handle.model_id != request.model_id {
                return Err(MlxError::ModelNotFound(request.model_id.clone()));
            }
            if !state.active_requests.insert(request.request_id.clone()) {
                return Err(MlxError::RequestAlreadyRunning(request.request_id.clone()));
            }
            handle
        };
        let mut validating =
            ValidatingStreamSink::new(sink, &request.request_id, self.config.max_stream_bytes);
        let result = run_bounded(
            context,
            "stream",
            self.controller
                .stream(&handle, request, &mut validating, context),
        )
        .await;
        lock(&self.state)?
            .active_requests
            .remove(&request.request_id);
        let summary = result?;
        validating.finish(&summary)?;
        Ok(summary)
    }

    pub async fn cancel_generation(
        &self,
        request_id: &str,
        context: &MlxOperationContext,
    ) -> MlxResult<bool> {
        validate_id(request_id, "requestId")?;
        validate_context(context)?;
        let handle = {
            let state = lock(&self.state)?;
            if !state.active_requests.contains(request_id) {
                return Ok(false);
            }
            state.running.clone().ok_or(MlxError::NotRunning)?
        };
        run_bounded(
            context,
            "cancel generation",
            self.controller.cancel(&handle, request_id),
        )
        .await?;
        Ok(true)
    }

    pub async fn metrics(&self, context: &MlxOperationContext) -> MlxResult<MlxProcessMetrics> {
        validate_context(context)?;
        self.probe.probe()?.ensure_available()?;
        let handle = lock(&self.state)?
            .running
            .clone()
            .ok_or(MlxError::NotRunning)?;
        run_bounded(
            context,
            "metrics",
            self.controller.inspect(&handle, context),
        )
        .await
    }

    pub async fn tail_logs(
        &self,
        max_bytes: usize,
        context: &MlxOperationContext,
    ) -> MlxResult<String> {
        validate_context(context)?;
        if max_bytes == 0 || max_bytes > self.config.max_log_bytes {
            return Err(limit(
                "MLX log bytes",
                max_bytes as u64,
                self.config.max_log_bytes as u64,
            ));
        }
        let handle = lock(&self.state)?
            .running
            .clone()
            .ok_or(MlxError::NotRunning)?;
        let logs = run_bounded(
            context,
            "tail logs",
            self.controller.tail_logs(&handle, max_bytes, context),
        )
        .await?;
        if logs.len() > max_bytes {
            return Err(limit("MLX log bytes", logs.len() as u64, max_bytes as u64));
        }
        Ok(logs)
    }

    pub async fn unload(&self, context: &MlxOperationContext) -> MlxResult<bool> {
        validate_context(context)?;
        self.probe.probe()?.ensure_available()?;
        let (handle, requests) = {
            let state = lock(&self.state)?;
            let Some(handle) = state.running.clone() else {
                return Ok(false);
            };
            (
                handle,
                state.active_requests.iter().cloned().collect::<Vec<_>>(),
            )
        };
        for request_id in &requests {
            run_bounded(
                context,
                "cancel generation for unload",
                self.controller.cancel(&handle, request_id),
            )
            .await?;
        }
        let timeout = self.config.unload_timeout_ms.min(MAX_MLX_UNLOAD_WAIT_MS);
        let metrics = run_bounded(
            context,
            "unload",
            self.controller
                .terminate_and_wait(&handle, timeout, context),
        )
        .await?;
        if metrics.process_alive
            || metrics.resident_memory_bytes != 0
            || metrics.unified_memory_bytes != 0
        {
            return Err(MlxError::Timeout {
                operation: "unload and release memory".to_string(),
                timeout_ms: timeout,
            });
        }
        let mut state = lock(&self.state)?;
        if state.running.as_ref().map(|running| &running.process_id) == Some(&handle.process_id) {
            state.running = None;
            state.active_requests.clear();
        }
        Ok(true)
    }
}

struct ValidatingStreamSink<'a> {
    inner: &'a mut dyn MlxStreamSink,
    request_id: &'a str,
    max_bytes: usize,
    observed_bytes: usize,
    started: bool,
    terminal: bool,
}

impl<'a> ValidatingStreamSink<'a> {
    fn new(inner: &'a mut dyn MlxStreamSink, request_id: &'a str, max_bytes: usize) -> Self {
        Self {
            inner,
            request_id,
            max_bytes,
            observed_bytes: 0,
            started: false,
            terminal: false,
        }
    }

    fn finish(&self, summary: &MlxGenerationSummary) -> MlxResult<()> {
        if !self.started || !self.terminal || summary.request_id != self.request_id {
            return Err(MlxError::StreamProtocol(
                "stream must start and terminate exactly once for the requested id".to_string(),
            ));
        }
        Ok(())
    }
}

impl MlxStreamSink for ValidatingStreamSink<'_> {
    fn emit(&mut self, event: MlxStreamEvent) -> Result<(), String> {
        if self.terminal {
            return Err("event arrived after terminal event".to_string());
        }
        match &event {
            MlxStreamEvent::Started { request_id }
                if !self.started && request_id == self.request_id =>
            {
                self.started = true;
            }
            MlxStreamEvent::Started { .. } => return Err("invalid duplicate/start id".to_string()),
            MlxStreamEvent::Completed { .. } | MlxStreamEvent::Error { .. } if self.started => {
                self.terminal = true;
            }
            _ if !self.started => return Err("delta arrived before start".to_string()),
            _ => {}
        }
        let bytes = serde_json::to_vec(&event).map_err(|error| error.to_string())?;
        self.observed_bytes = self.observed_bytes.saturating_add(bytes.len());
        if self.observed_bytes > self.max_bytes {
            return Err("stream exceeded configured byte limit".to_string());
        }
        self.inner.emit(event)
    }
}

async fn run_bounded<T>(
    context: &MlxOperationContext,
    operation: &str,
    future: MlxFuture<'_, T>,
) -> MlxResult<T> {
    if context.cancellation.is_cancelled() {
        return Err(MlxError::Cancelled {
            operation: operation.to_string(),
        });
    }
    tokio::select! {
        _ = context.cancellation.cancelled() => Err(MlxError::Cancelled {
            operation: operation.to_string(),
        }),
        result = tokio::time::timeout(std::time::Duration::from_millis(context.timeout_ms), future) => {
            result.map_err(|_| MlxError::Timeout {
                operation: operation.to_string(),
                timeout_ms: context.timeout_ms,
            })?
        }
    }
}

fn validate_manifest(
    manifest: &MlxPackageManifest,
    limits: &MlxInstallLimits,
    host: &MlxHostCapabilities,
) -> MlxResult<()> {
    validate_manifest_target_shape(manifest, limits)?;
    if normalize_os(&manifest.target_os) != host.os
        || normalize_arch(&manifest.target_architecture) != host.architecture
    {
        return Err(invalid(
            "manifest.target",
            "package target does not match the probed host",
        ));
    }
    Ok(())
}

fn validate_manifest_target_shape(
    manifest: &MlxPackageManifest,
    limits: &MlxInstallLimits,
) -> MlxResult<()> {
    if manifest.schema_version != MLX_PACKAGE_SCHEMA_VERSION {
        return Err(invalid("manifest.schemaVersion", "is unsupported"));
    }
    validate_id(&manifest.package_version, "manifest.packageVersion")?;
    if normalize_os(&manifest.target_os) != "macos"
        || normalize_arch(&manifest.target_architecture) != "aarch64"
    {
        return Err(invalid(
            "manifest.target",
            "MLX packages must explicitly target macOS/aarch64",
        ));
    }
    validate_relative_path(&manifest.python_executable, "manifest.pythonExecutable")?;
    validate_relative_path(&manifest.service_entry, "manifest.serviceEntry")?;
    validate_id(&manifest.signature_algorithm, "manifest.signatureAlgorithm")?;
    validate_id(&manifest.signature_key_id, "manifest.signatureKeyId")?;
    if manifest.signature_algorithm.eq_ignore_ascii_case("none") {
        return Err(invalid(
            "manifest.signatureAlgorithm",
            "unsigned packages are forbidden",
        ));
    }
    if manifest.files.is_empty() || manifest.files.len() > limits.max_files {
        return Err(limit(
            "MLX package file count",
            manifest.files.len() as u64,
            limits.max_files as u64,
        ));
    }
    let mut previous = None::<&str>;
    let mut total = 0_u64;
    let mut python_found = false;
    let mut service_found = false;
    for file in &manifest.files {
        validate_relative_path(&file.path, "manifest.files[].path")?;
        if previous.is_some_and(|value| value >= file.path.as_str()) {
            return Err(invalid(
                "manifest.files",
                "files must be unique and sorted by path",
            ));
        }
        previous = Some(&file.path);
        validate_sha256(&file.sha256, "manifest.files[].sha256")?;
        if file.size_bytes > limits.max_file_bytes {
            return Err(limit(
                "MLX package file bytes",
                file.size_bytes,
                limits.max_file_bytes,
            ));
        }
        total = total
            .checked_add(file.size_bytes)
            .ok_or_else(|| invalid("manifest.files", "total size overflow"))?;
        if file.path == manifest.python_executable {
            python_found = file.executable;
        }
        if file.path == manifest.service_entry {
            service_found = true;
        }
    }
    if total > limits.max_total_bytes {
        return Err(limit(
            "MLX package total bytes",
            total,
            limits.max_total_bytes,
        ));
    }
    if !python_found || !service_found {
        return Err(invalid(
            "manifest",
            "Python executable and service entry must be declared; Python must be executable",
        ));
    }
    Ok(())
}

fn validate_install_limits(limits: &MlxInstallLimits) -> MlxResult<()> {
    if limits.max_files == 0
        || limits.max_file_bytes == 0
        || limits.max_total_bytes == 0
        || limits.max_manifest_bytes == 0
    {
        Err(invalid("installLimits", "all limits must be positive"))
    } else {
        Ok(())
    }
}

fn validate_runtime_config(config: &MlxRuntimeConfig) -> MlxResult<()> {
    validate_id(&config.runtime_id, "runtimeId")?;
    if config.port == 0 {
        return Err(invalid("port", "must be non-zero"));
    }
    if config.unload_timeout_ms == 0 || config.unload_timeout_ms > MAX_MLX_UNLOAD_WAIT_MS {
        return Err(invalid(
            "unloadTimeoutMs",
            "must be between 1 and 10000 milliseconds",
        ));
    }
    if config.max_log_bytes == 0 || config.max_stream_bytes == 0 {
        return Err(invalid("runtimeLimits", "byte limits must be positive"));
    }
    Ok(())
}

fn validate_context(context: &MlxOperationContext) -> MlxResult<()> {
    if context.timeout_ms == 0 || context.timeout_ms > 15 * 60 * 1_000 {
        Err(invalid(
            "context.timeoutMs",
            "must be between 1 ms and 15 minutes",
        ))
    } else if context.cancellation.is_cancelled() {
        Err(MlxError::Cancelled {
            operation: "preflight".to_string(),
        })
    } else {
        Ok(())
    }
}

fn validate_model(model: &MlxModelRecord) -> MlxResult<()> {
    validate_id(&model.model_id, "modelId")?;
    validate_text(&model.display_name, "displayName", 4_096)?;
    if !model.local_path.is_absolute() || model.size_bytes == 0 {
        return Err(invalid(
            "model",
            "local path must be absolute and size must be positive",
        ));
    }
    if !model.capabilities.chat {
        return Err(invalid(
            "model.capabilities.chat",
            "must be true for MLX chat",
        ));
    }
    Ok(())
}

fn validate_generation_request(
    request: &MlxGenerationRequest,
    model: &MlxModelRecord,
) -> MlxResult<()> {
    validate_id(&request.request_id, "requestId")?;
    validate_id(&request.model_id, "modelId")?;
    if request.messages.is_empty() || request.messages.len() > 100_000 {
        return Err(invalid("messages", "must contain 1..=100000 messages"));
    }
    let mut text_bytes = 0_usize;
    for message in &request.messages {
        if !matches!(
            message.role.as_str(),
            "system" | "user" | "assistant" | "tool"
        ) {
            return Err(invalid("messages[].role", "is unsupported"));
        }
        validate_text(&message.text, "messages[].text", MAX_TEXT_BYTES)?;
        text_bytes = text_bytes.saturating_add(message.text.len());
    }
    if text_bytes > MAX_TEXT_BYTES {
        return Err(limit(
            "MLX prompt bytes",
            text_bytes as u64,
            MAX_TEXT_BYTES as u64,
        ));
    }
    if request.max_tokens == 0 || request.max_tokens > 1_000_000 {
        return Err(invalid("maxTokens", "must be between 1 and 1000000"));
    }
    if request
        .temperature
        .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
    {
        return Err(invalid("temperature", "must be finite and between 0 and 2"));
    }
    if !request.tools.is_empty() && !model.capabilities.tool_calling {
        return Err(invalid(
            "tools",
            "selected model does not support tool calling",
        ));
    }
    if request.structured_output_schema.is_some() && !model.capabilities.structured_output {
        return Err(invalid(
            "structuredOutputSchema",
            "selected model does not support structured output",
        ));
    }
    if request.tools.len() > 128 {
        return Err(invalid("tools", "at most 128 tools are accepted"));
    }
    let mut names = BTreeSet::new();
    for tool in &request.tools {
        validate_id(&tool.name, "tools[].name")?;
        validate_text(&tool.description, "tools[].description", 64 * 1024)?;
        if !tool.input_schema.is_object() || !names.insert(tool.name.as_str()) {
            return Err(invalid(
                "tools",
                "tool names must be unique and input schemas must be objects",
            ));
        }
    }
    Ok(())
}

fn validate_launch_spec(spec: &MlxLaunchSpec) -> MlxResult<()> {
    validate_id(&spec.runtime_id, "launch.runtimeId")?;
    validate_id(&spec.model_id, "launch.modelId")?;
    if !spec.program.is_absolute() || spec.port == 0 || spec.args.len() > 128 {
        return Err(invalid(
            "launch",
            "requires an absolute program, non-zero port, and bounded argument vector",
        ));
    }
    if spec
        .args
        .iter()
        .any(|argument| argument.len() > MAX_PATH_BYTES || argument.contains('\0'))
    {
        return Err(invalid("launch.args", "contains an unsafe argument"));
    }
    Ok(())
}

fn validate_handle(
    handle: &MlxProcessHandle,
    config: &MlxRuntimeConfig,
    model_id: &str,
) -> MlxResult<()> {
    validate_id(&handle.process_id, "processId")?;
    if handle.port != config.port || handle.model_id != model_id || handle.started_at_ms == 0 {
        return Err(MlxError::Controller {
            operation: "start".to_string(),
            message: "controller returned a handle for a different port/model or zero timestamp"
                .to_string(),
        });
    }
    Ok(())
}

fn reject_unexpected_install_entries(
    root: &Path,
    directory: &Path,
    expected_paths: &BTreeSet<String>,
) -> MlxResult<()> {
    for entry in fs::read_dir(directory)
        .map_err(|source| io_at("list installed MLX package", directory, source))?
    {
        let entry = entry.map_err(|source| io_at("read installed entry", directory, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_at("inspect installed entry", &path, source))?;
        if metadata.file_type().is_symlink() {
            return Err(invalid("install", "symlinks are forbidden"));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid("install", "entry escaped install root"))?;
        let relative_text = relative
            .to_str()
            .ok_or_else(|| invalid("install", "entry path is not UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if metadata.is_dir() {
            reject_unexpected_install_entries(root, &path, expected_paths)?;
        } else if metadata.is_file()
            && relative_text != INSTALL_MANIFEST_FILE
            && !expected_paths.contains(&relative_text)
        {
            return Err(invalid(
                "install",
                format!("unexpected installed file {relative_text}"),
            ));
        } else if !metadata.is_file() {
            return Err(invalid("install", "special files are forbidden"));
        }
    }
    Ok(())
}

fn validate_relative_path(path: &str, field: &str) -> MlxResult<()> {
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.contains('\0')
        || path.contains('\\')
        || path.contains(':')
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(invalid(field, "must be a safe normalized relative path"));
    }
    for component in Path::new(path).components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(invalid(field, "contains traversal"));
        }
    }
    Ok(())
}

fn validate_id(value: &str, field: &str) -> MlxResult<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        Err(invalid(
            field,
            format!("must contain 1..={MAX_ID_BYTES} bytes without controls"),
        ))
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, field: &str, max: usize) -> MlxResult<()> {
    if value.len() > max || value.contains('\0') {
        Err(invalid(
            field,
            format!("exceeds {max} bytes or contains NUL"),
        ))
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str, field: &str) -> MlxResult<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(invalid(field, "must be lowercase hexadecimal SHA-256"))
    }
}

fn decode_signature(value: &str) -> MlxResult<Vec<u8>> {
    if value.is_empty() || value.len() > 16 * 1024 {
        return Err(invalid("signatureBase64", "is empty or oversized"));
    }
    let signature = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| invalid("signatureBase64", error.to_string()))?;
    if signature.len() < 32 || signature.len() > 8 * 1024 {
        return Err(invalid(
            "signatureBase64",
            "decoded signature must contain 32..=8192 bytes",
        ));
    }
    Ok(signature)
}

fn version_directory_name(version: &str, digest: &str) -> String {
    let safe_version = version
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("{safe_version}-{}", &digest[..16])
}

fn read_regular_bounded(path: &Path, max_bytes: usize) -> MlxResult<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_at("inspect MLX file", path, source))?;
    if !metadata.file_type().is_file() {
        return Err(invalid("file", "must be a real regular file"));
    }
    if metadata.len() > max_bytes as u64 {
        return Err(limit("MLX file bytes", metadata.len(), max_bytes as u64));
    }
    let bytes = fs::read(path).map_err(|source| io_at("read MLX file", path, source))?;
    if bytes.len() > max_bytes {
        return Err(limit(
            "MLX file bytes",
            bytes.len() as u64,
            max_bytes as u64,
        ));
    }
    Ok(bytes)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> MlxResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("path", "has no parent"))?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(".mlx-write-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|source| io_at("create atomic MLX file", &temporary, source))?;
    if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(io_at("write atomic MLX file", &temporary, source));
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_at("publish atomic MLX file", path, source));
    }
    sync_directory(parent)
}

fn ensure_private_directory(path: &Path) -> MlxResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(invalid("directory", "is not a real directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| io_at("create private MLX directory", path, source))?,
        Err(source) => return Err(io_at("inspect private MLX directory", path, source)),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_at("secure private MLX directory", path, source))?;
    Ok(())
}

fn harden_directory_tree(path: &Path, stop: &Path) -> MlxResult<()> {
    let mut current = Some(path);
    while let Some(directory) = current {
        #[cfg(unix)]
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .map_err(|source| io_at("secure package directory", directory, source))?;
        if directory == stop {
            break;
        }
        current = directory.parent();
    }
    Ok(())
}

fn sync_directory(path: &Path) -> MlxResult<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_at("sync MLX directory", path, source))?;
    Ok(())
}

fn canonical_json<T: Serialize + ?Sized>(value: &T) -> MlxResult<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    canonicalize(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize),
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut sorted = previous.into_iter().collect::<Vec<_>>();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in sorted {
                canonicalize(&mut value);
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

fn normalize_os(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "darwin" | "mac" | "macos" => "macos".to_string(),
        "win" | "windows" | "win32" => "windows".to_string(),
        "linux" => "linux".to_string(),
        other => other.to_string(),
    }
}

fn normalize_arch(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "aarch64".to_string(),
        "amd64" | "x64" | "x86_64" => "x86_64".to_string(),
        other => other.to_string(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> MlxResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| MlxError::LockPoisoned)
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> MlxError {
    MlxError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

fn limit(name: &'static str, observed: u64, max: u64) -> MlxError {
    MlxError::Limit {
        name,
        observed,
        max,
    }
}

fn io_at(operation: &'static str, path: &Path, source: io::Error) -> MlxError {
    MlxError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!("mlx-{label}-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) struct TestSignatureVerifier;

    impl TestSignatureVerifier {
        pub(crate) fn sign(key_id: &str, payload: &[u8]) -> Vec<u8> {
            let mut hash = Sha256::new();
            hash.update(key_id.as_bytes());
            hash.update(payload);
            hash.finalize().to_vec()
        }
    }

    impl MlxSignatureVerifier for TestSignatureVerifier {
        fn verify(
            &self,
            algorithm: &str,
            key_id: &str,
            signed_payload: &[u8],
            signature: &[u8],
        ) -> Result<(), String> {
            if algorithm != "test-sha256-contract"
                || signature != Self::sign(key_id, signed_payload)
            {
                Err("signature mismatch".to_string())
            } else {
                Ok(())
            }
        }
    }

    fn supported_host() -> MlxHostCapabilities {
        MlxHostCapabilities::evaluate("macos", "arm64", true, Some(32 << 30), Some(24 << 30))
    }

    fn package() -> MlxPackageBundle {
        let mut files = BTreeMap::new();
        files.insert("bin/python".to_string(), b"private-python-runtime".to_vec());
        files.insert(
            "service/mlx_server.py".to_string(),
            b"print('mlx service')".to_vec(),
        );
        let package_files = files
            .iter()
            .map(|(path, bytes)| MlxPackageFile {
                path: path.clone(),
                size_bytes: bytes.len() as u64,
                sha256: sha256_hex(bytes),
                executable: path == "bin/python",
            })
            .collect::<Vec<_>>();
        let mut manifest = MlxPackageManifest {
            schema_version: MLX_PACKAGE_SCHEMA_VERSION,
            package_version: "mlx-0.25.2+python-3.12".to_string(),
            target_os: "macos".to_string(),
            target_architecture: "aarch64".to_string(),
            python_executable: "bin/python".to_string(),
            service_entry: "service/mlx_server.py".to_string(),
            files: package_files,
            signature_algorithm: "test-sha256-contract".to_string(),
            signature_key_id: "publisher-key-2026".to_string(),
            signature_base64: String::new(),
        };
        let payload = canonical_json(&UnsignedMlxPackageManifest::from(&manifest))
            .expect("canonical unsigned manifest");
        manifest.signature_base64 = base64::engine::general_purpose::STANDARD.encode(
            TestSignatureVerifier::sign(&manifest.signature_key_id, &payload),
        );
        MlxPackageBundle { manifest, files }
    }

    pub(crate) fn write_test_signed_archive(
        path: &Path,
        package_version: &str,
        valid_signature: bool,
    ) {
        let mut bundle = package();
        bundle.manifest.package_version = package_version.to_string();
        let mut entropy = 0x9e37_79b9_u32;
        let padding = (0..(3 * 64 * 1024 + 1))
            .map(|_| {
                entropy ^= entropy << 13;
                entropy ^= entropy >> 17;
                entropy ^= entropy << 5;
                entropy as u8
            })
            .collect::<Vec<_>>();
        bundle
            .files
            .insert("support/padding.bin".to_string(), padding.clone());
        bundle.manifest.files.push(MlxPackageFile {
            path: "support/padding.bin".to_string(),
            size_bytes: padding.len() as u64,
            sha256: sha256_hex(&padding),
            executable: false,
        });
        let payload = canonical_json(&UnsignedMlxPackageManifest::from(&bundle.manifest))
            .expect("canonical unsigned manifest");
        bundle.manifest.signature_base64 = base64::engine::general_purpose::STANDARD.encode(
            TestSignatureVerifier::sign(&bundle.manifest.signature_key_id, &payload),
        );
        if !valid_signature {
            bundle.manifest.signature_base64 = base64::engine::general_purpose::STANDARD
                .encode(b"invalid deterministic publisher signature");
        }

        let encoder = flate2::write::GzEncoder::new(
            fs::File::create(path).expect("create test archive"),
            flate2::Compression::fast(),
        );
        let mut archive = tar::Builder::new(encoder);
        let manifest = serde_json::to_vec(&bundle.manifest).expect("serialize manifest");
        let mut append = |name: &str, bytes: &[u8], mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            archive
                .append_data(&mut header, name, bytes)
                .expect("append test archive member");
        };
        append("mlx-package.json", &manifest, 0o644);
        for (name, bytes) in &bundle.files {
            append(
                name,
                bytes,
                if name == "bin/python" { 0o755 } else { 0o644 },
            );
        }
        archive
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip");
    }

    /// Both canonicalizers must agree byte for byte, or packages built by the
    /// script verify nowhere.
    ///
    /// The signature covers exactly these bytes. `scripts/lib/mlxPackage.mjs`
    /// produces them in JavaScript and its own test asserts the identical
    /// string, so a change to either implementation breaks one of the two
    /// tests. Without that pairing the failure surfaces only as an installer
    /// reporting "signature is invalid" for a package that was signed
    /// correctly — indistinguishable from a tampered one.
    #[test]
    fn canonical_manifest_bytes_match_the_packaging_script() {
        const FIXTURE: &str = concat!(
            r#"{"files":[{"executable":true,"path":"bin/python","sha256":"#,
            r#""0000000000000000000000000000000000000000000000000000000000000000","#,
            r#""sizeBytes":3}],"packageVersion":"mlx-0.1.0","pythonExecutable":"bin/python","#,
            r#""schemaVersion":1,"serviceEntry":"service/mlx_server.py","#,
            r#""signatureAlgorithm":"ed25519","signatureKeyId":"release-2026-1","#,
            r#""targetArchitecture":"aarch64","targetOs":"macos"}"#
        );
        let manifest = MlxPackageManifest {
            schema_version: 1,
            package_version: "mlx-0.1.0".to_string(),
            target_os: "macos".to_string(),
            target_architecture: "aarch64".to_string(),
            python_executable: "bin/python".to_string(),
            service_entry: "service/mlx_server.py".to_string(),
            files: vec![MlxPackageFile {
                path: "bin/python".to_string(),
                size_bytes: 3,
                sha256: "0".repeat(64),
                executable: true,
            }],
            signature_algorithm: "ed25519".to_string(),
            signature_key_id: "release-2026-1".to_string(),
            // Present, and still absent from the signed bytes: the unsigned
            // view drops the key rather than blanking it.
            signature_base64: "must-not-appear".to_string(),
        };
        let payload =
            canonical_json(&UnsignedMlxPackageManifest::from(&manifest)).expect("canonical");
        assert_eq!(String::from_utf8(payload).unwrap(), FIXTURE);
    }

    /// A built package directory is read into exactly the bundle the installer
    /// takes — and a directory is not trusted for what it contains, only for
    /// what its signed manifest declares.
    #[test]
    fn a_package_directory_is_read_into_an_installable_bundle() {
        let root = std::env::temp_dir().join(format!("mlx-src-{}", Uuid::new_v4().simple()));
        let bundle = package();
        for (path, bytes) in &bundle.files {
            let full = root.join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, bytes).unwrap();
        }
        // A file nobody signed for. It must not travel with the bundle.
        fs::write(root.join("service/extra.py"), b"print('unsigned')").unwrap();
        fs::write(
            root.join(MLX_SOURCE_MANIFEST_FILE),
            canonical_json(&bundle.manifest).unwrap(),
        )
        .unwrap();

        let limits = MlxInstallLimits::default();
        let read = read_package_directory(&root, &limits).expect("read package directory");
        assert_eq!(read.manifest, bundle.manifest);
        assert_eq!(read.files, bundle.files);
        assert!(
            !read.files.contains_key("service/extra.py"),
            "only manifest-declared files are packaged"
        );
        // ...and the round trip really installs, which is the whole point of
        // reading it in this shape.
        let installer = installer(&root.join("installed"));
        installer
            .install_and_activate(&read, &supported_host())
            .expect("install the directory that was read");

        // A directory whose bytes no longer match its manifest is refused
        // before anything is written.
        fs::write(root.join("service/mlx_server.py"), b"print('swapped')").unwrap();
        let tampered = read_package_directory(&root, &limits).expect("still reads");
        assert!(installer
            .install_and_activate(&tampered, &supported_host())
            .is_err());

        fs::remove_dir_all(&root).unwrap();
    }

    /// A package archive is untrusted input, and unpacking is where that bites.
    ///
    /// The digest the component hub checks proves the bytes match the catalog;
    /// it proves nothing about what the catalog listed. So a traversal entry, a
    /// symlink, or an absolute path has to be refused by the extractor itself —
    /// by the time the signature is checked, a careless unpack has already
    /// written outside the tree.
    #[test]
    fn a_hostile_archive_cannot_write_outside_the_package_root() {
        let base = std::env::temp_dir().join(format!("mlx-tar-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&base).unwrap();
        let limits = MlxInstallLimits::default();

        // Names are written straight into the header rather than through
        // `append_data`, whose own guard rejects `..` before it is written.
        // An attacker has no such guard — the bytes on the wire are whatever
        // they chose — so a test that could only produce well-formed names
        // would be testing the tar crate's writer, not this extractor.
        let build = |name: &str, entries: Vec<(&str, &[u8])>| -> PathBuf {
            let path = base.join(name);
            let encoder = flate2::write::GzEncoder::new(
                File::create(&path).unwrap(),
                flate2::Compression::fast(),
            );
            let mut archive = tar::Builder::new(encoder);
            for (entry_path, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_entry_type(tar::EntryType::Regular);
                let raw = entry_path.as_bytes();
                header.as_gnu_mut().unwrap().name[..raw.len()].copy_from_slice(raw);
                header.set_cksum();
                archive.append(&header, bytes).unwrap();
            }
            archive.into_inner().unwrap().finish().unwrap();
            path
        };

        // The ordinary case still works, or the checks below prove nothing.
        let good = build(
            "good.tar.gz",
            vec![("service/mlx_server.py", b"print(1)" as &[u8])],
        );
        let out = base.join("good-out");
        extract_package_archive(&good, &out, &limits).expect("a normal archive unpacks");
        assert_eq!(
            fs::read(out.join("service/mlx_server.py")).unwrap(),
            b"print(1)"
        );

        // Escapes, refused before any write reaches the parent.
        let canary = base.join("pwned.txt");
        for (name, entry) in [
            ("dotdot.tar.gz", "../pwned.txt"),
            ("nested.tar.gz", "service/../../pwned.txt"),
            ("absolute.tar.gz", "/tmp/pwned.txt"),
        ] {
            let archive = build(name, vec![(entry, b"owned" as &[u8])]);
            assert!(
                extract_package_archive(&archive, &base.join(name), &limits).is_err(),
                "{entry} must be refused"
            );
            assert!(!canary.exists(), "{entry} wrote outside the package root");
        }

        // A symlink entry is how a *later* entry gets redirected out of the
        // tree, so the entry type is refused rather than followed.
        let link = base.join("link.tar.gz");
        {
            let encoder = flate2::write::GzEncoder::new(
                File::create(&link).unwrap(),
                flate2::Compression::fast(),
            );
            let mut archive = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_mode(0o777);
            archive
                .append_link(&mut header, "escape", "/tmp/pwned.txt")
                .unwrap();
            archive.into_inner().unwrap().finish().unwrap();
        }
        assert!(
            extract_package_archive(&link, &base.join("link-out"), &limits).is_err(),
            "a symlink entry must be refused, not followed"
        );

        // Bounds are enforced from the headers, before the bytes are written.
        let many = build(
            "many.tar.gz",
            (0..5).map(|_| ("f", b"x" as &[u8])).collect::<Vec<_>>(),
        );
        let tight = MlxInstallLimits {
            max_files: 2,
            ..MlxInstallLimits::default()
        };
        assert!(extract_package_archive(&many, &base.join("many-out"), &tight).is_err());

        fs::remove_dir_all(&base).unwrap();
    }

    /// Installs a real package built by `pnpm mlx:package`.
    ///
    /// Ignored by default because it needs a built tree — a bundled
    /// interpreter and mlx-lm, ten thousand files and a gigabyte — but it is
    /// the only check that runs the actual artifact through the actual
    /// installer, digests and Ed25519 and all. The fixtures above prove the
    /// logic; this proves the packaging script and this module agree.
    ///
    /// ```text
    /// MLX_SIGNING_KEY=… pnpm mlx:package
    /// MLX_PACKAGE_DIR=packaging/mlx/dist MLX_PACKAGE_PUBKEY_HEX=… \
    ///   cargo test --lib mlx_runtime -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "needs a package built by pnpm mlx:package"]
    fn a_real_built_package_installs_and_verifies() {
        let (Ok(directory), Ok(public_key_hex)) = (
            std::env::var("MLX_PACKAGE_DIR"),
            std::env::var("MLX_PACKAGE_PUBKEY_HEX"),
        ) else {
            panic!("set MLX_PACKAGE_DIR and MLX_PACKAGE_PUBKEY_HEX");
        };

        struct RealKeyVerifier(Vec<u8>);
        impl MlxSignatureVerifier for RealKeyVerifier {
            fn verify(
                &self,
                algorithm: &str,
                _key_id: &str,
                payload: &[u8],
                signature: &[u8],
            ) -> Result<(), String> {
                if algorithm != "ed25519" {
                    return Err(format!("unexpected algorithm {algorithm}"));
                }
                ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &self.0)
                    .verify(payload, signature)
                    .map_err(|_| "signature is invalid".to_string())
            }
        }

        let key = (0..public_key_hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&public_key_hex[at..at + 2], 16).expect("hex"))
            .collect::<Vec<_>>();
        let limits = MlxInstallLimits::default();
        let bundle =
            read_package_directory(Path::new(&directory), &limits).expect("read built package");
        println!(
            "read {} files, version {}",
            bundle.files.len(),
            bundle.manifest.package_version
        );

        let root = std::env::temp_dir().join(format!("mlx-real-{}", Uuid::new_v4().simple()));
        let installer = Arc::new(
            MlxPackageInstaller::new(&root, Arc::new(RealKeyVerifier(key)), limits)
                .expect("installer"),
        );
        let installed = installer
            .install_and_activate(&bundle, &supported_host())
            .expect("install the real package");
        // The interpreter and the entry point are what the runtime execs, so
        // an install that does not leave both on disk installed nothing usable.
        assert!(installed.python_executable.is_file());
        assert!(installed.service_entry.is_file());
        installer.verify_active().expect("re-verify from disk");
        println!("installed and re-verified {}", installed.package_version);
        fs::remove_dir_all(&root).unwrap();
    }

    fn installer(root: &Path) -> Arc<MlxPackageInstaller> {
        Arc::new(
            MlxPackageInstaller::new(
                root,
                Arc::new(TestSignatureVerifier),
                MlxInstallLimits::default(),
            )
            .expect("installer"),
        )
    }

    #[test]
    fn capability_matrix_never_advertises_mlx_off_apple_silicon() {
        let supported = supported_host();
        assert!(supported.is_available());
        assert_eq!(supported.os, "macos");
        assert_eq!(supported.architecture, "aarch64");

        for (os, arch, metal, reason) in [
            ("linux", "aarch64", true, MlxUnavailableReason::NonMacOs),
            ("windows", "x86_64", true, MlxUnavailableReason::NonMacOs),
            (
                "macos",
                "x86_64",
                true,
                MlxUnavailableReason::NonAppleSilicon,
            ),
            (
                "darwin",
                "arm64",
                false,
                MlxUnavailableReason::MetalUnavailable,
            ),
        ] {
            let host = MlxHostCapabilities::evaluate(os, arch, metal, None, None);
            assert!(!host.is_available());
            assert_eq!(host.unavailable_reason, Some(reason));
            assert!(matches!(
                host.ensure_available(),
                Err(MlxError::Unavailable { .. })
            ));
        }
    }

    #[test]
    fn installer_rejects_tamper_and_atomically_verifies_active_runtime() {
        let directory = TestDirectory::new("install");
        let installer = installer(&directory.0);
        let host = supported_host();

        let mut bad_signature = package();
        bad_signature.manifest.signature_base64 =
            base64::engine::general_purpose::STANDARD.encode([0_u8; 32]);
        assert!(matches!(
            installer.install_and_activate(&bad_signature, &host),
            Err(MlxError::Signature(_))
        ));
        assert!(!directory.0.join(ACTIVE_STATE_FILE).exists());

        let mut bad_digest = package();
        bad_digest
            .files
            .get_mut("service/mlx_server.py")
            .expect("service")
            .push(0);
        assert!(matches!(
            installer.install_and_activate(&bad_digest, &host),
            Err(MlxError::Invalid { .. }) | Err(MlxError::DigestMismatch { .. })
        ));

        let bundle = package();
        let installed = installer
            .install_and_activate(&bundle, &host)
            .expect("verified install");
        assert!(installed.python_executable.starts_with(&directory.0));
        assert!(installed.service_entry.starts_with(&directory.0));
        assert_eq!(installer.verify_active().expect("active verify"), installed);
        assert_eq!(
            installer
                .install_and_activate(&bundle, &host)
                .expect("idempotent install"),
            installed
        );

        fs::write(&installed.service_entry, b"tampered").expect("tamper installed service");
        assert!(matches!(
            installer.verify_active(),
            Err(MlxError::Invalid { .. }) | Err(MlxError::DigestMismatch { .. })
        ));
    }

    #[derive(Clone)]
    struct StaticProbe(MlxHostCapabilities);

    impl MlxCapabilityProbe for StaticProbe {
        fn probe(&self) -> MlxResult<MlxHostCapabilities> {
            Ok(self.0.clone())
        }
    }

    struct MockController {
        launch_calls: AtomicUsize,
        inspect_calls: AtomicUsize,
        terminate_calls: AtomicUsize,
        cancel_calls: Mutex<Vec<String>>,
        process_alive: AtomicBool,
        block_stream: bool,
        stream_started: AtomicBool,
        stream_release: Notify,
    }

    impl MockController {
        fn new(block_stream: bool) -> Self {
            Self {
                launch_calls: AtomicUsize::new(0),
                inspect_calls: AtomicUsize::new(0),
                terminate_calls: AtomicUsize::new(0),
                cancel_calls: Mutex::new(Vec::new()),
                process_alive: AtomicBool::new(true),
                block_stream,
                stream_started: AtomicBool::new(false),
                stream_release: Notify::new(),
            }
        }

        fn running_metrics() -> MlxProcessMetrics {
            MlxProcessMetrics {
                process_alive: true,
                resident_memory_bytes: 2 << 30,
                unified_memory_bytes: 3 << 30,
                active_requests: 0,
                generated_tokens: 42,
                tokens_per_second: Some(18.5),
                sampled_at_ms: 10_100,
            }
        }
    }

    impl MlxServiceController for MockController {
        fn port_owner<'a>(&'a self, _port: u16) -> MlxFuture<'a, Option<String>> {
            Box::pin(async { Ok(None) })
        }

        fn launch<'a>(
            &'a self,
            spec: MlxLaunchSpec,
            _context: &'a MlxOperationContext,
        ) -> MlxFuture<'a, MlxProcessHandle> {
            self.launch_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert!(spec.program.is_absolute());
                assert_eq!(spec.args[1..5], ["--host", "127.0.0.1", "--port", "8081"]);
                Ok(MlxProcessHandle {
                    process_id: "mlx-process-1".to_string(),
                    os_pid: Some(77),
                    port: spec.port,
                    model_id: spec.model_id,
                    started_at_ms: 10_000,
                })
            })
        }

        fn inspect<'a>(
            &'a self,
            _handle: &'a MlxProcessHandle,
            _context: &'a MlxOperationContext,
        ) -> MlxFuture<'a, MlxProcessMetrics> {
            self.inspect_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let mut metrics = Self::running_metrics();
                if !self.process_alive.load(Ordering::SeqCst) {
                    metrics.process_alive = false;
                    metrics.resident_memory_bytes = 0;
                    metrics.unified_memory_bytes = 0;
                }
                Ok(metrics)
            })
        }

        fn stream<'a>(
            &'a self,
            _handle: &'a MlxProcessHandle,
            request: &'a MlxGenerationRequest,
            sink: &'a mut dyn MlxStreamSink,
            _context: &'a MlxOperationContext,
        ) -> MlxFuture<'a, MlxGenerationSummary> {
            Box::pin(async move {
                sink.emit(MlxStreamEvent::Started {
                    request_id: request.request_id.clone(),
                })
                .map_err(|message| MlxError::Controller {
                    operation: "stream sink".to_string(),
                    message,
                })?;
                self.stream_started.store(true, Ordering::SeqCst);
                if self.block_stream {
                    self.stream_release.notified().await;
                    if lock(&self.cancel_calls)?.contains(&request.request_id) {
                        return Err(MlxError::Cancelled {
                            operation: "stream".to_string(),
                        });
                    }
                }
                sink.emit(MlxStreamEvent::TextDelta {
                    text: "hello".to_string(),
                })
                .and_then(|_| {
                    sink.emit(MlxStreamEvent::Completed {
                        input_tokens: 5,
                        output_tokens: 1,
                    })
                })
                .map_err(|message| MlxError::Controller {
                    operation: "stream sink".to_string(),
                    message,
                })?;
                Ok(MlxGenerationSummary {
                    request_id: request.request_id.clone(),
                    input_tokens: 5,
                    output_tokens: 1,
                    finish_reason: "stop".to_string(),
                })
            })
        }

        fn cancel<'a>(
            &'a self,
            _handle: &'a MlxProcessHandle,
            request_id: &'a str,
        ) -> MlxFuture<'a, ()> {
            Box::pin(async move {
                lock(&self.cancel_calls)?.push(request_id.to_string());
                self.stream_release.notify_waiters();
                Ok(())
            })
        }

        fn terminate_and_wait<'a>(
            &'a self,
            _handle: &'a MlxProcessHandle,
            timeout_ms: u64,
            _context: &'a MlxOperationContext,
        ) -> MlxFuture<'a, MlxProcessMetrics> {
            self.terminate_calls.fetch_add(1, Ordering::SeqCst);
            self.process_alive.store(false, Ordering::SeqCst);
            Box::pin(async move {
                assert!(timeout_ms <= MAX_MLX_UNLOAD_WAIT_MS);
                Ok(MlxProcessMetrics {
                    process_alive: false,
                    resident_memory_bytes: 0,
                    unified_memory_bytes: 0,
                    active_requests: 0,
                    generated_tokens: 42,
                    tokens_per_second: None,
                    sampled_at_ms: 10_200,
                })
            })
        }

        fn tail_logs<'a>(
            &'a self,
            _handle: &'a MlxProcessHandle,
            max_bytes: usize,
            _context: &'a MlxOperationContext,
        ) -> MlxFuture<'a, String> {
            Box::pin(async move {
                let logs = "ready\nstreamed\n";
                Ok(logs[..logs.len().min(max_bytes)].to_string())
            })
        }
    }

    /// An absolute path valid on whichever OS this actually runs under.
    /// `/foo` satisfies `Path::is_absolute()` on Unix but not on Windows
    /// (which requires a drive-letter or UNC prefix) — [`validate_model`]
    /// checks exactly that, and this fixture never touches real disk I/O, so
    /// any platform-appropriate absolute path is equally valid here.
    fn fixture_absolute_path(rest: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(format!(r"C:\{}", rest.replace('/', "\\")))
        } else {
            PathBuf::from(format!("/{rest}"))
        }
    }

    fn model() -> MlxModelRecord {
        MlxModelRecord {
            model_id: "mlx-community/Qwen3-4B-4bit".to_string(),
            display_name: "Qwen3 4B MLX".to_string(),
            local_path: fixture_absolute_path("private/models/qwen3"),
            size_bytes: 3 << 30,
            revision: Some("abc123".to_string()),
            capabilities: MlxModelCapabilities {
                chat: true,
                tool_calling: true,
                vision: false,
                structured_output: true,
            },
        }
    }

    fn request(id: &str) -> MlxGenerationRequest {
        MlxGenerationRequest {
            request_id: id.to_string(),
            model_id: model().model_id,
            messages: vec![MlxMessage {
                role: "user".to_string(),
                text: "Hello".to_string(),
                images: Vec::new(),
            }],
            tools: Vec::new(),
            max_tokens: 32,
            temperature: Some(0.2),
            structured_output_schema: None,
        }
    }

    struct VecSink(Vec<MlxStreamEvent>);

    impl MlxStreamSink for VecSink {
        fn emit(&mut self, event: MlxStreamEvent) -> Result<(), String> {
            self.0.push(event);
            Ok(())
        }
    }

    fn adapter(
        root: &Path,
        host: MlxHostCapabilities,
        controller: Arc<MockController>,
    ) -> Arc<MlxRuntimeAdapter> {
        let installer = installer(root);
        if host.is_available() {
            installer
                .install_and_activate(&package(), &host)
                .expect("install runtime for adapter");
        }
        Arc::new(
            MlxRuntimeAdapter::new(
                MlxRuntimeConfig::default(),
                Arc::new(StaticProbe(host)),
                installer,
                controller,
                vec![model()],
            )
            .expect("adapter"),
        )
    }

    #[tokio::test]
    async fn lifecycle_stream_metrics_logs_and_unload_are_wired() {
        let directory = TestDirectory::new("lifecycle");
        let controller = Arc::new(MockController::new(false));
        let adapter = adapter(&directory.0, supported_host(), controller.clone());
        let context = MlxOperationContext::default();
        let handle = adapter
            .start(&model().model_id, &context)
            .await
            .expect("start MLX");
        assert_eq!(controller.launch_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            adapter
                .start(&model().model_id, &context)
                .await
                .expect("idempotent start"),
            handle
        );

        let mut sink = VecSink(Vec::new());
        let summary = adapter
            .stream(&request("request-1"), &mut sink, &context)
            .await
            .expect("stream");
        assert_eq!(summary.output_tokens, 1);
        assert!(matches!(sink.0[0], MlxStreamEvent::Started { .. }));
        assert!(matches!(sink.0[1], MlxStreamEvent::TextDelta { .. }));
        assert!(matches!(sink.0[2], MlxStreamEvent::Completed { .. }));
        assert_eq!(
            adapter
                .metrics(&context)
                .await
                .expect("metrics")
                .generated_tokens,
            42
        );
        assert_eq!(
            adapter.tail_logs(32, &context).await.expect("logs"),
            "ready\nstreamed\n"
        );
        assert!(matches!(
            adapter.status(&context).await,
            Ok(MlxRuntimeStatus::Running { .. })
        ));
        assert!(adapter.unload(&context).await.expect("unload"));
        assert_eq!(controller.terminate_calls.load(Ordering::SeqCst), 1);
        assert!(!adapter.unload(&context).await.expect("idempotent unload"));
    }

    #[tokio::test]
    async fn dead_process_handles_are_reconciled_before_status_or_restart() {
        let directory = TestDirectory::new("stale-handle");
        let controller = Arc::new(MockController::new(false));
        let adapter = adapter(&directory.0, supported_host(), controller.clone());
        let context = MlxOperationContext::default();
        let first = adapter
            .start(&model().model_id, &context)
            .await
            .expect("initial start");
        controller.process_alive.store(false, Ordering::SeqCst);
        assert!(matches!(
            adapter.status(&context).await.expect("reconciled status"),
            MlxRuntimeStatus::Stopped { .. }
        ));

        controller.process_alive.store(true, Ordering::SeqCst);
        let second = adapter
            .start(&model().model_id, &context)
            .await
            .expect("restart stale process");
        assert_eq!(first.model_id, second.model_id);
        assert_eq!(controller.launch_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn active_generation_can_be_cancelled_and_state_is_released() {
        let directory = TestDirectory::new("cancel");
        let controller = Arc::new(MockController::new(true));
        let adapter = adapter(&directory.0, supported_host(), controller.clone());
        let context = MlxOperationContext::default();
        adapter
            .start(&model().model_id, &context)
            .await
            .expect("start");
        let task_adapter = adapter.clone();
        let task = tokio::spawn(async move {
            let mut sink = VecSink(Vec::new());
            task_adapter
                .stream(
                    &request("request-cancel"),
                    &mut sink,
                    &MlxOperationContext::default(),
                )
                .await
        });
        for _ in 0..1_000 {
            if controller.stream_started.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(controller.stream_started.load(Ordering::SeqCst));
        assert!(adapter
            .cancel_generation("request-cancel", &context)
            .await
            .expect("cancel"));
        assert!(matches!(
            task.await.expect("join stream"),
            Err(MlxError::Cancelled { .. })
        ));
        assert!(!adapter
            .cancel_generation("request-cancel", &context)
            .await
            .expect("already finished"));
        assert!(adapter.unload(&context).await.expect("unload"));
    }

    #[tokio::test]
    async fn unsupported_hosts_fail_before_install_or_process_side_effects() {
        let directory = TestDirectory::new("unsupported");
        let controller = Arc::new(MockController::new(false));
        let host = MlxHostCapabilities::evaluate("linux", "aarch64", true, None, None);
        let adapter = adapter(&directory.0, host, controller.clone());
        let error = adapter
            .start(&model().model_id, &MlxOperationContext::default())
            .await
            .expect_err("Linux must be explicitly unavailable");
        assert!(matches!(error, MlxError::Unavailable { .. }));
        assert_eq!(controller.launch_calls.load(Ordering::SeqCst), 0);
        assert!(matches!(
            adapter.status(&MlxOperationContext::default()).await,
            Ok(MlxRuntimeStatus::Unavailable { .. })
        ));
    }

    #[test]
    fn request_capabilities_fail_before_controller_submission() {
        let mut no_tools = model();
        no_tools.capabilities.tool_calling = false;
        let mut with_tool = request("request-tool");
        with_tool.tools.push(MlxToolDefinition {
            name: "lookup".to_string(),
            description: "Lookup".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        });
        assert!(matches!(
            validate_generation_request(&with_tool, &no_tools),
            Err(MlxError::Invalid { field, .. }) if field == "tools"
        ));

        let mut bad_temperature = request("request-temperature");
        bad_temperature.temperature = Some(f64::NAN);
        assert!(validate_generation_request(&bad_temperature, &model()).is_err());
    }
}
