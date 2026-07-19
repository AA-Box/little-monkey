//! Durable content-addressed artifact storage.
//!
//! Blobs are identified by the lowercase hexadecimal SHA-256 digest of their
//! exact bytes and stored under `blobs/<first two>/<next two>/<digest>`. Writes
//! are staged in a sibling temporary directory, flushed to stable storage,
//! and atomically renamed into place. The store deliberately has no Tauri or
//! async dependency so the same integrity contract can be reused by the app,
//! CLI, background runner, exports, and tests.

use serde::Serialize;
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// A defensive default for a single durable blob. Callers that own a tighter
/// budget can use [`ArtifactStore::with_max_blob_size`].
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 256 * 1024 * 1024;

const BLOBS_DIRECTORY: &str = "blobs";
const TEMP_DIRECTORY: &str = ".tmp";
const SHA256_HEX_LENGTH: usize = 64;
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const READ_REPLACEMENT_RETRIES: usize = 64;

pub type ArtifactStoreResult<T> = Result<T, ArtifactStoreError>;

/// Metadata that can be trusted only after the corresponding bytes have been
/// written or read through [`ArtifactStore`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactBlob {
    pub id: String,
    pub size: u64,
}

#[derive(Debug)]
pub enum ArtifactStoreError {
    InvalidBlobId {
        id: String,
    },
    BlobTooLarge {
        max: u64,
        observed: u64,
    },
    BlobNotFound {
        id: String,
    },
    UnsafeFileType {
        path: PathBuf,
    },
    SourceChanged {
        path: PathBuf,
    },
    SizeMismatch {
        id: String,
        expected: u64,
        actual: u64,
    },
    DigestMismatch {
        expected: String,
        actual: String,
    },
    InputRead {
        source: io::Error,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ArtifactStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBlobId { id } => write!(f, "invalid SHA-256 blob id: {id:?}"),
            Self::BlobTooLarge { max, observed } => {
                write!(f, "blob is {observed} bytes, exceeding the {max} byte limit")
            }
            Self::BlobNotFound { id } => write!(f, "artifact blob {id} does not exist"),
            Self::UnsafeFileType { path } => write!(
                f,
                "refusing to follow or store a non-regular file: {}",
                path.display()
            ),
            Self::SourceChanged { path } => {
                write!(f, "source changed while it was being imported: {}", path.display())
            }
            Self::SizeMismatch { id, expected, actual } => write!(
                f,
                "artifact blob {id} changed size while being read (expected {expected}, read {actual})"
            ),
            Self::DigestMismatch { expected, actual } => write!(
                f,
                "artifact blob digest mismatch (path says {expected}, content hashes to {actual})"
            ),
            Self::InputRead { source } => write!(f, "failed to read artifact input: {source}"),
            Self::Io { operation, path, source } => {
                write!(f, "failed to {operation} {}: {source}", path.display())
            }
        }
    }
}

impl Error for ArtifactStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InputRead { source } | Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// One non-fatal problem found by [`ArtifactStore::scan_integrity`]. The scan
/// continues after individual corrupt, misplaced, or unreadable entries so a
/// repair UI can present the complete set in one pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityIssue {
    pub path: PathBuf,
    pub blob_id: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    pub checked_blobs: usize,
    pub valid_blobs: usize,
    pub valid_bytes: u64,
    pub issues: Vec<IntegrityIssue>,
}

impl IntegrityReport {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

/// A cloneable handle to one on-disk content-addressed store. No process-local
/// lock is required: unique staging files plus atomic rename make concurrent
/// writers of identical content converge on the same final path.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    blobs_dir: PathBuf,
    temp_dir: PathBuf,
    max_blob_bytes: u64,
}

impl ArtifactStore {
    pub fn new(root: impl AsRef<Path>) -> ArtifactStoreResult<Self> {
        Self::with_max_blob_size(root, DEFAULT_MAX_BLOB_BYTES)
    }

    pub fn with_max_blob_size(
        root: impl AsRef<Path>,
        max_blob_bytes: u64,
    ) -> ArtifactStoreResult<Self> {
        let root = root.as_ref().to_path_buf();
        create_store_root(&root)?;
        let blobs_dir = root.join(BLOBS_DIRECTORY);
        let temp_dir = root.join(TEMP_DIRECTORY);
        ensure_directory(&blobs_dir)?;
        ensure_directory(&temp_dir)?;
        Ok(Self {
            root,
            blobs_dir,
            temp_dir,
            max_blob_bytes,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn max_blob_size(&self) -> u64 {
        self.max_blob_bytes
    }

    /// Returns the deterministic sharded path for a validated blob id. The
    /// strict lowercase-hex validation is the traversal boundary: no caller
    /// input is ever appended to the store root before it passes this check.
    pub fn blob_path(&self, id: &str) -> ArtifactStoreResult<PathBuf> {
        validate_blob_id(id)?;
        Ok(self.blobs_dir.join(&id[..2]).join(&id[2..4]).join(id))
    }

    pub fn put(&self, bytes: &[u8]) -> ArtifactStoreResult<ArtifactBlob> {
        let observed = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if observed > self.max_blob_bytes {
            return Err(ArtifactStoreError::BlobTooLarge {
                max: self.max_blob_bytes,
                observed,
            });
        }
        self.put_reader(io::Cursor::new(bytes))
    }

    pub fn put_reader<R: Read>(&self, reader: R) -> ArtifactStoreResult<ArtifactBlob> {
        let staged = self.stage_reader(reader)?;
        self.publish_staged(staged)
    }

    /// Alias that makes the ingestion intent explicit at connector/file
    /// boundaries while retaining the same bounded streaming implementation.
    pub fn import_reader<R: Read>(&self, reader: R) -> ArtifactStoreResult<ArtifactBlob> {
        self.put_reader(reader)
    }

    /// Imports a regular file without accepting a symlink as the source. The
    /// source identity, size, and modification time are rechecked around the
    /// copy so a rename/replacement race cannot silently publish different
    /// bytes than the file the caller selected.
    pub fn import_file(&self, source: impl AsRef<Path>) -> ArtifactStoreResult<ArtifactBlob> {
        let source = source.as_ref();
        let before = metadata_without_symlink(source, "inspect artifact source")?;
        if before.len() > self.max_blob_bytes {
            return Err(ArtifactStoreError::BlobTooLarge {
                max: self.max_blob_bytes,
                observed: before.len(),
            });
        }

        let mut file =
            File::open(source).map_err(|error| io_at("open artifact source", source, error))?;
        let opened = file
            .metadata()
            .map_err(|error| io_at("inspect opened artifact source", source, error))?;
        if !opened.is_file() || !same_file_identity(&before, &opened) {
            return Err(ArtifactStoreError::SourceChanged {
                path: source.to_path_buf(),
            });
        }

        let staged = self.stage_reader(&mut file)?;
        let after = file
            .metadata()
            .map_err(|error| io_at("reinspect artifact source", source, error))?;
        if !same_file_snapshot(&opened, &after) || staged.size != opened.len() {
            return Err(ArtifactStoreError::SourceChanged {
                path: source.to_path_buf(),
            });
        }
        self.publish_staged(staged)
    }

    /// Reads and verifies a blob. Both the file length observed before/after
    /// reading and the SHA-256 digest must match; callers never receive bytes
    /// from a truncated, replaced, oversized, or tampered entry.
    pub fn read(&self, id: &str) -> ArtifactStoreResult<Vec<u8>> {
        for attempt in 0..=READ_REPLACEMENT_RETRIES {
            match self.read_once(id) {
                Err(ArtifactStoreError::SourceChanged { .. })
                    if attempt < READ_REPLACEMENT_RETRIES =>
                {
                    // Concurrent same-digest publishers can atomically replace
                    // one verified inode with another on Unix. Retry until the
                    // finite publication burst settles; no unverified bytes are
                    // ever returned from an individual attempt.
                    std::thread::yield_now();
                }
                result => return result,
            }
        }
        unreachable!("the bounded read retry loop always returns on its final attempt")
    }

    fn read_once(&self, id: &str) -> ArtifactStoreResult<Vec<u8>> {
        let path = self.blob_path(id)?;
        let before = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(ArtifactStoreError::BlobNotFound { id: id.to_string() });
            }
            Err(error) => return Err(io_at("inspect artifact blob", &path, error)),
        };
        if before.file_type().is_symlink() || !before.is_file() {
            return Err(ArtifactStoreError::UnsafeFileType { path });
        }
        if before.len() > self.max_blob_bytes {
            return Err(ArtifactStoreError::BlobTooLarge {
                max: self.max_blob_bytes,
                observed: before.len(),
            });
        }

        let mut file =
            File::open(&path).map_err(|error| io_at("open artifact blob", &path, error))?;
        let opened = file
            .metadata()
            .map_err(|error| io_at("inspect opened artifact blob", &path, error))?;
        if !opened.is_file() || !same_file_identity(&before, &opened) {
            return Err(ArtifactStoreError::SourceChanged { path });
        }

        let mut bytes = Vec::new();
        let limit = self.max_blob_bytes.saturating_add(1);
        {
            let mut bounded = (&mut file).take(limit);
            bounded
                .read_to_end(&mut bytes)
                .map_err(|error| io_at("read artifact blob", &path, error))?;
        }
        let actual_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if actual_size > self.max_blob_bytes {
            return Err(ArtifactStoreError::BlobTooLarge {
                max: self.max_blob_bytes,
                observed: actual_size,
            });
        }
        let after = file
            .metadata()
            .map_err(|error| io_at("reinspect artifact blob", &path, error))?;
        if !same_file_snapshot(&opened, &after) || actual_size != opened.len() {
            return Err(ArtifactStoreError::SizeMismatch {
                id: id.to_string(),
                expected: opened.len(),
                actual: actual_size,
            });
        }

        let actual_digest = sha256_hex(&bytes);
        if actual_digest != id {
            return Err(ArtifactStoreError::DigestMismatch {
                expected: id.to_string(),
                actual: actual_digest,
            });
        }
        Ok(bytes)
    }

    /// Reports whether the validated blob path currently contains a regular
    /// file. This is intentionally cheaper than [`Self::read`]; use read or
    /// the integrity scan when cryptographic verification is required.
    pub fn exists(&self, id: &str) -> ArtifactStoreResult<bool> {
        let path = self.blob_path(id)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(ArtifactStoreError::UnsafeFileType { path })
            }
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(io_at("inspect artifact blob", &path, error)),
        }
    }

    /// Deletes one regular blob. Missing blobs are a successful no-op and
    /// return `false`; invalid ids and unexpected file types remain errors.
    pub fn delete(&self, id: &str) -> ArtifactStoreResult<bool> {
        let path = self.blob_path(id)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(ArtifactStoreError::UnsafeFileType { path });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_at("inspect artifact blob", &path, error)),
        }
        fs::remove_file(&path).map_err(|error| io_at("delete artifact blob", &path, error))?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(true)
    }

    /// Verifies every entry in the expected two-level shard layout without
    /// following directory or file symlinks. Layout and per-blob failures are
    /// accumulated in the report; failure to open the store root itself is a
    /// hard error.
    pub fn scan_integrity(&self) -> ArtifactStoreResult<IntegrityReport> {
        let metadata =
            metadata_without_symlink(&self.blobs_dir, "inspect artifact blob directory")?;
        if !metadata.is_dir() {
            return Err(ArtifactStoreError::UnsafeFileType {
                path: self.blobs_dir.clone(),
            });
        }
        let mut report = IntegrityReport::default();
        self.scan_shard_directory(&self.blobs_dir, 0, None, &mut report, true)?;
        Ok(report)
    }

    fn stage_reader<R: Read>(&self, mut reader: R) -> ArtifactStoreResult<StagedBlob> {
        let (mut file, temp) = self.create_temp_file()?;
        let outcome = (|| -> ArtifactStoreResult<(String, u64)> {
            let mut hasher = Sha256::new();
            let mut total = 0_u64;
            let mut buffer = [0_u8; COPY_BUFFER_BYTES];

            loop {
                let read = reader
                    .read(&mut buffer)
                    .map_err(|source| ArtifactStoreError::InputRead { source })?;
                if read == 0 {
                    break;
                }
                let read_u64 = u64::try_from(read).unwrap_or(u64::MAX);
                total = total.saturating_add(read_u64);
                if total > self.max_blob_bytes {
                    return Err(ArtifactStoreError::BlobTooLarge {
                        max: self.max_blob_bytes,
                        observed: total,
                    });
                }
                file.write_all(&buffer[..read])
                    .map_err(|error| io_at("write staged artifact blob", temp.path(), error))?;
                hasher.update(&buffer[..read]);
            }
            file.sync_all()
                .map_err(|error| io_at("flush staged artifact blob", temp.path(), error))?;
            Ok((digest_hex(hasher.finalize().as_slice()), total))
        })();
        drop(file);
        let (id, size) = outcome?;
        Ok(StagedBlob { temp, id, size })
    }

    fn create_temp_file(&self) -> ArtifactStoreResult<(File, TempPath)> {
        for _ in 0..16 {
            let path = self
                .temp_dir
                .join(format!("{}.tmp", Uuid::new_v4().simple()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((file, TempPath::new(path))),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(io_at("create staged artifact blob", &path, error)),
            }
        }
        Err(io_at(
            "create staged artifact blob",
            &self.temp_dir,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique temporary file",
            ),
        ))
    }

    fn publish_staged(&self, mut staged: StagedBlob) -> ArtifactStoreResult<ArtifactBlob> {
        let path = self.blob_path(&staged.id)?;
        let parent = path
            .parent()
            .expect("a sharded artifact path always has a parent");
        let first_shard = parent
            .parent()
            .expect("a two-level sharded artifact path always has a first shard");
        ensure_directory(first_shard)?;
        ensure_directory(parent)?;

        match self.read(&staged.id) {
            Ok(existing) => {
                return Ok(ArtifactBlob {
                    id: staged.id,
                    size: u64::try_from(existing.len()).unwrap_or(u64::MAX),
                });
            }
            Err(ArtifactStoreError::BlobNotFound { .. }) => {}
            Err(error) => return Err(error),
        }

        if let Err(rename_error) = fs::rename(staged.temp.path(), &path) {
            // Windows does not replace an existing destination. If a racing
            // writer published the same verified digest first, that is the
            // desired deduplicated result rather than a failed write.
            match self.read(&staged.id) {
                Ok(existing) => {
                    return Ok(ArtifactBlob {
                        id: staged.id,
                        size: u64::try_from(existing.len()).unwrap_or(u64::MAX),
                    });
                }
                Err(ArtifactStoreError::BlobNotFound { .. }) => {
                    return Err(io_at("publish artifact blob", &path, rename_error));
                }
                Err(error) => return Err(error),
            }
        }
        staged.temp.disarm();
        sync_directory(parent)?;
        sync_directory(&self.temp_dir)?;

        let verified = self.read(&staged.id)?;
        let verified_size = u64::try_from(verified.len()).unwrap_or(u64::MAX);
        if verified_size != staged.size {
            return Err(ArtifactStoreError::SizeMismatch {
                id: staged.id,
                expected: staged.size,
                actual: verified_size,
            });
        }
        Ok(ArtifactBlob {
            id: staged.id,
            size: staged.size,
        })
    }

    fn scan_shard_directory(
        &self,
        directory: &Path,
        depth: u8,
        first_shard: Option<&str>,
        report: &mut IntegrityReport,
        root: bool,
    ) -> ArtifactStoreResult<()> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if root => return Err(io_at("scan artifact directory", directory, error)),
            Err(error) => {
                push_issue(
                    report,
                    directory,
                    None,
                    format!("cannot read shard directory: {error}"),
                );
                return Ok(());
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_issue(
                        report,
                        directory,
                        None,
                        format!("cannot read directory entry: {error}"),
                    );
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    push_issue(
                        report,
                        &path,
                        None,
                        format!("cannot inspect entry type: {error}"),
                    );
                    continue;
                }
            };

            if depth < 2 {
                if file_type.is_symlink() || !file_type.is_dir() {
                    push_issue(
                        report,
                        &path,
                        None,
                        "expected a real shard directory".to_string(),
                    );
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    push_issue(
                        report,
                        &path,
                        None,
                        "shard name is not valid UTF-8".to_string(),
                    );
                    continue;
                };
                if !valid_shard_name(&name) {
                    push_issue(
                        report,
                        &path,
                        None,
                        "invalid shard directory name".to_string(),
                    );
                    continue;
                }
                let first = if depth == 0 {
                    Some(name.as_str())
                } else {
                    first_shard
                };
                self.scan_shard_directory(&path, depth + 1, first, report, false)?;
                continue;
            }

            report.checked_blobs += 1;
            if file_type.is_symlink() || !file_type.is_file() {
                push_issue(
                    report,
                    &path,
                    None,
                    "expected a regular artifact blob".to_string(),
                );
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                push_issue(
                    report,
                    &path,
                    None,
                    "blob filename is not valid UTF-8".to_string(),
                );
                continue;
            };
            if let Err(error) = validate_blob_id(&id) {
                push_issue(report, &path, Some(id), error.to_string());
                continue;
            }
            let second_shard = directory.file_name().and_then(|name| name.to_str());
            if first_shard != Some(&id[..2]) || second_shard != Some(&id[2..4]) {
                push_issue(
                    report,
                    &path,
                    Some(id),
                    "blob is stored under the wrong shard path".to_string(),
                );
                continue;
            }
            match self.read(&id) {
                Ok(bytes) => {
                    report.valid_blobs += 1;
                    report.valid_bytes = report
                        .valid_bytes
                        .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
                }
                Err(error) => push_issue(report, &path, Some(id), error.to_string()),
            }
        }
        Ok(())
    }
}

struct StagedBlob {
    temp: TempPath,
    id: String,
    size: u64,
}

struct TempPath {
    path: PathBuf,
    armed: bool,
}

impl TempPath {
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

impl Drop for TempPath {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn create_store_root(path: &Path) -> ArtifactStoreResult<()> {
    fs::create_dir_all(path).map_err(|error| io_at("create artifact store", path, error))?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_at("inspect artifact store", path, error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ArtifactStoreError::UnsafeFileType {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn ensure_directory(path: &Path) -> ArtifactStoreResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(ArtifactStoreError::UnsafeFileType {
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
            Err(error) => Err(io_at("create artifact directory", path, error)),
        },
        Err(error) => Err(io_at("inspect artifact directory", path, error)),
    }
}

fn metadata_without_symlink(path: &Path, operation: &'static str) -> ArtifactStoreResult<Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io_at(operation, path, error))?;
    if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
        return Err(ArtifactStoreError::UnsafeFileType {
            path: path.to_path_buf(),
        });
    }
    Ok(metadata)
}

fn validate_blob_id(id: &str) -> ArtifactStoreResult<()> {
    if id.len() == SHA256_HEX_LENGTH
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(ArtifactStoreError::InvalidBlobId { id: id.to_string() })
    }
}

fn valid_shard_name(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest_hex(Sha256::digest(bytes).as_slice())
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
fn sync_directory(path: &Path) -> ArtifactStoreResult<()> {
    let directory =
        File::open(path).map_err(|error| io_at("open directory for sync", path, error))?;
    directory
        .sync_all()
        .map_err(|error| io_at("sync artifact directory", path, error))
}

// Stable file handles are flushed on every platform. Rust's standard library
// does not expose a portable Windows directory handle suitable for fsync;
// rename still provides atomic publication there.
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> ArtifactStoreResult<()> {
    Ok(())
}

fn push_issue(report: &mut IntegrityReport, path: &Path, blob_id: Option<String>, message: String) {
    report.issues.push(IntegrityIssue {
        path: path.to_path_buf(),
        blob_id,
        message,
    });
}

fn io_at(operation: &'static str, path: impl AsRef<Path>, source: io::Error) -> ArtifactStoreError {
    ArtifactStoreError::Io {
        operation,
        path: path.as_ref().to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-artifact-store-{}-{}",
                std::process::id(),
                Uuid::new_v4().simple()
            ));
            fs::create_dir(&path).expect("create test directory");
            Self { path }
        }

        fn store_path(&self) -> PathBuf {
            self.path.join("store")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn store(test_dir: &TestDir) -> ArtifactStore {
        ArtifactStore::with_max_blob_size(test_dir.store_path(), 1024 * 1024).unwrap()
    }

    #[test]
    fn rejects_traversal_and_noncanonical_ids_before_building_paths() {
        let test_dir = TestDir::new();
        let store = store(&test_dir);
        for invalid in [
            "",
            "../outside",
            "aa/bb",
            "g000000000000000000000000000000000000000000000000000000000000000",
            "A000000000000000000000000000000000000000000000000000000000000000",
        ] {
            assert!(matches!(
                store.blob_path(invalid),
                Err(ArtifactStoreError::InvalidBlobId { .. })
            ));
            assert!(matches!(
                store.exists(invalid),
                Err(ArtifactStoreError::InvalidBlobId { .. })
            ));
        }
    }

    #[test]
    fn deduplicates_identical_content_into_one_sharded_blob() {
        let test_dir = TestDir::new();
        let store = store(&test_dir);
        let first = store.put(b"same bytes").unwrap();
        let second = store.import_reader(io::Cursor::new(b"same bytes")).unwrap();

        assert_eq!(first, second);
        assert_eq!(store.read(&first.id).unwrap(), b"same bytes");
        let path = store.blob_path(&first.id).unwrap();
        assert_eq!(path.parent().unwrap().file_name().unwrap(), &first.id[2..4]);
        assert_eq!(
            path.parent()
                .unwrap()
                .parent()
                .unwrap()
                .file_name()
                .unwrap(),
            &first.id[..2]
        );
        let report = store.scan_integrity().unwrap();
        assert_eq!(report.checked_blobs, 1);
        assert_eq!(report.valid_blobs, 1);
        assert!(report.is_clean());
    }

    #[test]
    fn detects_tampering_on_read_and_in_the_full_integrity_scan() {
        let test_dir = TestDir::new();
        let store = store(&test_dir);
        let blob = store.put(b"trusted content").unwrap();
        fs::write(store.blob_path(&blob.id).unwrap(), b"tampered content").unwrap();

        assert!(matches!(
            store.read(&blob.id),
            Err(ArtifactStoreError::DigestMismatch { .. })
        ));
        let report = store.scan_integrity().unwrap();
        assert_eq!(report.checked_blobs, 1);
        assert_eq!(report.valid_blobs, 0);
        assert_eq!(report.issues.len(), 1);
        assert_eq!(report.issues[0].blob_id.as_deref(), Some(blob.id.as_str()));
    }

    #[test]
    fn rejects_oversized_bytes_and_streams_without_leaving_temp_files() {
        let test_dir = TestDir::new();
        let store = ArtifactStore::with_max_blob_size(test_dir.store_path(), 4).unwrap();

        assert!(matches!(
            store.put(b"12345"),
            Err(ArtifactStoreError::BlobTooLarge {
                max: 4,
                observed: 5
            })
        ));
        assert!(matches!(
            store.put_reader(io::Cursor::new(b"abcdef")),
            Err(ArtifactStoreError::BlobTooLarge { max: 4, .. })
        ));
        assert_eq!(fs::read_dir(&store.temp_dir).unwrap().count(), 0);
        assert!(store.scan_integrity().unwrap().is_clean());
    }

    #[test]
    fn concurrent_same_content_puts_converge_atomically() {
        let test_dir = TestDir::new();
        let store = Arc::new(store(&test_dir));
        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                store.put(b"published concurrently")
            }));
        }
        let blobs: Vec<ArtifactBlob> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().unwrap())
            .collect();

        assert!(blobs.iter().all(|blob| blob == &blobs[0]));
        assert_eq!(store.read(&blobs[0].id).unwrap(), b"published concurrently");
        let report = store.scan_integrity().unwrap();
        assert_eq!((report.checked_blobs, report.valid_blobs), (1, 1));
        assert!(report.is_clean());
        assert_eq!(fs::read_dir(&store.temp_dir).unwrap().count(), 0);
    }

    #[test]
    fn reopening_the_same_root_preserves_verified_content() {
        let test_dir = TestDir::new();
        let store_path = test_dir.store_path();
        let blob = {
            let store = ArtifactStore::new(&store_path).unwrap();
            store.put(b"survives reopen").unwrap()
        };
        let reopened = ArtifactStore::new(&store_path).unwrap();

        assert!(reopened.exists(&blob.id).unwrap());
        assert_eq!(reopened.read(&blob.id).unwrap(), b"survives reopen");
    }

    #[test]
    fn imports_regular_files_and_rejects_symlink_sources() {
        let test_dir = TestDir::new();
        let store = store(&test_dir);
        let source = test_dir.path.join("source.bin");
        fs::write(&source, b"file import").unwrap();
        let blob = store.import_file(&source).unwrap();
        assert_eq!(store.read(&blob.id).unwrap(), b"file import");

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = test_dir.path.join("source-link.bin");
            symlink(&source, &link).unwrap();
            assert!(matches!(
                store.import_file(&link),
                Err(ArtifactStoreError::UnsafeFileType { .. })
            ));
        }
    }

    #[test]
    fn exists_and_delete_are_safe_and_idempotent() {
        let test_dir = TestDir::new();
        let store = store(&test_dir);
        let blob = store.put(b"delete me").unwrap();

        assert!(store.exists(&blob.id).unwrap());
        assert!(store.delete(&blob.id).unwrap());
        assert!(!store.exists(&blob.id).unwrap());
        assert!(!store.delete(&blob.id).unwrap());
        assert!(matches!(
            store.read(&blob.id),
            Err(ArtifactStoreError::BlobNotFound { .. })
        ));
    }
}
