//! Tauri-facing portability, encrypted snapshot, and WebDAV backup commands.
//! The archive/envelope validation stays in `portability`; this module owns
//! only app paths, keychain-backed production crypto, bounded file I/O, and
//! authenticated HTTP transport.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use futures_util::StreamExt;
use reqwest::header::{ETAG, IF_MATCH, IF_NONE_MATCH};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};
use url::Url;
use uuid::Uuid;

use crate::portability::{
    export_portable_bundle, export_session_docx, export_session_markdown, list_snapshot_files,
    open_encrypted_snapshot, preflight_portable_bundle, seal_encrypted_snapshot,
    write_snapshot_with_retention, CryptoProvider, CryptoSeal, EncryptedSnapshotEnvelopeV1,
    ImportLimits, ImportPreflightReport, PortableArtifactInput, PortableBundleInput,
    PortableDataV1, PortableSession, SnapshotFileInfo, SnapshotRetentionPolicy,
    SnapshotWriteOutcome,
};

const KEYCHAIN_SERVICE: &str = "com.littlemonkey.backup";
const BACKUP_KEY_ACCOUNT: &str = "portable-snapshot-aes256gcm-v1";
const BACKUP_CONFIG_FILE: &str = "backup_config.json";
const SNAPSHOT_DIRECTORY: &str = "backups";
const STAGED_SNAPSHOT_FILE: &str = "daemon-staged.lmsnapshot";
const ATTEMPT_SNAPSHOT_FILE: &str = ".webdav-attempt.lmsnapshot";
const ATTEMPT_JOURNAL_FILE: &str = ".webdav-attempt.json";
const BACKUP_SCHEDULER_DB: &str = "backup_scheduler.sqlite3";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_ATTEMPT_BYTES: u64 = 64 * 1024;
const MAX_EXPORT_ARTIFACTS: usize = 100_000;
const WEB_DAV_TIMEOUT_SECONDS: u64 = 45;
// Six bounded reconciliation/PUT requests can run in the worst conflict
// recovery path (6 * 45s). Keep a generous lease margin without allowing a
// crashed owner to block the next daemon launch indefinitely.
const BACKUP_CLAIM_TTL_MS: u64 = 10 * 60 * 1_000;
const BACKUP_RETRY_DELAY_MS: u64 = 60 * 1_000;
const RESTORE_TRANSACTION_PREFIX: &str = ".portable-restore-";
const RESTORE_JOURNAL_FILE: &str = "journal.json";
const RESTORE_COMMIT_MARKER: &str = "COMMITTED";
const RESTORE_ROLLBACK_PROFILE_FILE: &str = "rollback-profile.json";
const PENDING_RESTORE_SETTINGS_FILE: &str = "portable_restore_settings.json";
const MAX_PROMPTS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_STACK_REGISTRY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PENDING_SETTINGS_BYTES: u64 = 2 * 1024 * 1024;

fn restore_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn command_error(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(command_error)
        .and_then(|duration| u64::try_from(duration.as_millis()).map_err(command_error))
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableArtifactRequest {
    pub id: String,
    pub media_type: String,
    pub bytes_base64: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableBundleRequest {
    pub bundle_id: String,
    pub exported_at_ms: u64,
    pub app_version: String,
    pub data: PortableDataV1,
    #[serde(default)]
    pub artifacts: Vec<PortableArtifactRequest>,
}

impl PortableBundleRequest {
    fn into_core(self) -> Result<PortableBundleInput, String> {
        if self.artifacts.len() > MAX_EXPORT_ARTIFACTS {
            return Err(format!(
                "Portable export contains more than {MAX_EXPORT_ARTIFACTS} artifacts"
            ));
        }
        let mut total = 0_u64;
        let mut artifacts = Vec::with_capacity(self.artifacts.len());
        for artifact in self.artifacts {
            let estimated = artifact
                .bytes_base64
                .len()
                .checked_add(3)
                .and_then(|size| size.checked_div(4))
                .and_then(|size| size.checked_mul(3))
                .ok_or_else(|| "Portable artifact base64 size overflow".to_string())?;
            total = total
                .checked_add(u64::try_from(estimated).map_err(command_error)?)
                .ok_or_else(|| "Portable artifact total size overflow".to_string())?;
            if total > ImportLimits::default().max_total_expanded_bytes {
                return Err("Portable artifact payload exceeds the export safety limit".to_string());
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&artifact.bytes_base64)
                .map_err(|error| format!("Invalid artifact base64: {error}"))?;
            artifacts.push(PortableArtifactInput {
                id: artifact.id,
                media_type: artifact.media_type,
                bytes,
            });
        }
        Ok(PortableBundleInput {
            bundle_id: self.bundle_id,
            exported_at_ms: self.exported_at_ms,
            app_version: self.app_version,
            data: self.data,
            artifacts,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableArtifactResponse {
    pub id: String,
    pub media_type: String,
    pub bytes_base64: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableReadOutcome {
    pub data: PortableDataV1,
    pub artifacts: Vec<PortableArtifactResponse>,
    pub preflight: ImportPreflightReport,
}

fn read_regular_bounded(path: &Path, max_bytes: u64) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{} is {} bytes, above the {} byte limit",
            path.display(),
            metadata.len(),
            max_bytes
        ));
    }
    fs::read(path).map_err(|error| format!("Failed to read {}: {error}", path.display()))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Export destination has no parent directory".to_string())?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|error| format!("Failed to inspect {}: {error}", parent.display()))?;
    if !parent_metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", parent.display()));
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.file_type().is_file()) {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let staging = parent.join(format!(".little-monkey-export-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staging)
        .map_err(|error| format!("Failed to create {}: {error}", staging.display()))?;
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&staging);
        return Err(format!("Failed to write {}: {error}", staging.display()));
    }
    if let Err(error) = commit_atomic_replacement(&staging, path) {
        let _ = fs::remove_file(&staging);
        return Err(format!("Failed to publish {}: {error}", path.display()));
    }
    sync_directory(parent)?;
    Ok(())
}

#[cfg(unix)]
fn commit_atomic_replacement(staging: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(staging, destination)
}

#[cfg(windows)]
fn commit_atomic_replacement(staging: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = staging
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both owned buffers are NUL-terminated UTF-16 and live for the
    // full duration of this synchronous Win32 call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn commit_atomic_replacement(staging: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(staging, destination)
}

fn read_bundle_bytes(path: &Path) -> Result<Vec<u8>, String> {
    read_regular_bounded(path, ImportLimits::default().max_archive_bytes)
}

fn read_outcome(bytes: &[u8]) -> Result<PortableReadOutcome, String> {
    let (validated, preflight) =
        preflight_portable_bundle(bytes, &ImportLimits::default()).map_err(command_error)?;
    let media_types = validated
        .manifest
        .artifacts
        .iter()
        .map(|descriptor| (descriptor.id.clone(), descriptor.media_type.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let artifacts = validated
        .artifacts
        .into_iter()
        .map(|(id, bytes)| PortableArtifactResponse {
            media_type: media_types
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            id,
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect();
    Ok(PortableReadOutcome {
        data: validated.data,
        artifacts,
        preflight,
    })
}

// ---------------------------------------------------------------------------
// Atomic portable-profile restore
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PortableRestoreMode {
    Merge,
    Replace,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableRestoreSettingsRequest {
    pub locale: Option<String>,
    pub shortcut_overrides: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableRestoreRequest {
    pub mode: PortableRestoreMode,
    pub sessions_payload: String,
    pub previous_sessions_payload: String,
    pub prompts_payload: String,
    #[serde(default)]
    pub stacks: Vec<crate::knowledge_core::KnowledgeStack>,
    pub settings: Option<PortableRestoreSettingsRequest>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingPortableRestoreSettings {
    pub schema_version: u32,
    pub transaction_id: String,
    pub locale: Option<String>,
    pub shortcut_overrides: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableRestoreOutcome {
    pub transaction_id: String,
    pub stacks: Vec<crate::knowledge_core::KnowledgeStack>,
    pub profile_counts: crate::profile_store::ProfileCounts,
    pub settings_pending: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RestoreFileKind {
    Sessions,
    Prompts,
    Stacks,
    PendingSettings,
}

impl RestoreFileKind {
    fn stage_name(self) -> &'static str {
        match self {
            Self::Sessions => "sessions.new",
            Self::Prompts => "prompts.new",
            Self::Stacks => "stacks.new",
            Self::PendingSettings => "settings.new",
        }
    }

    fn backup_name(self) -> &'static str {
        match self {
            Self::Sessions => "sessions.backup",
            Self::Prompts => "prompts.backup",
            Self::Stacks => "stacks.backup",
            Self::PendingSettings => "settings.backup",
        }
    }

    fn max_bytes(self) -> u64 {
        match self {
            Self::Sessions => crate::profile_store::MAX_PROFILE_JSON_BYTES,
            Self::Prompts => MAX_PROMPTS_BYTES,
            Self::Stacks => MAX_STACK_REGISTRY_BYTES,
            Self::PendingSettings => MAX_PENDING_SETTINGS_BYTES,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreJournalEntry {
    kind: RestoreFileKind,
    had_original: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreJournal {
    schema_version: u32,
    transaction_id: String,
    files: Vec<RestoreJournalEntry>,
}

struct RestoreFilePlan {
    kind: RestoreFileKind,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct PublishedRestore {
    base: PathBuf,
    transaction_root: PathBuf,
    journal: RestoreJournal,
    stacks: Vec<crate::knowledge_core::KnowledgeStack>,
}

fn app_data_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let root = app.path().app_data_dir().map_err(command_error)?;
    fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create {}: {error}", root.display()))?;
    Ok(root)
}

fn restore_target(base: &Path, kind: RestoreFileKind) -> PathBuf {
    match kind {
        RestoreFileKind::Sessions => base.join("chat_sessions.json"),
        RestoreFileKind::Prompts => base.join("prompts.json"),
        RestoreFileKind::Stacks => base.join("stacks").join("index.json"),
        RestoreFileKind::PendingSettings => base.join(PENDING_RESTORE_SETTINGS_FILE),
    }
}

fn ensure_regular_or_missing(path: &Path, max_bytes: u64) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(metadata) if metadata.len() > max_bytes => Err(format!(
            "{} is {} bytes, above the {} byte restore limit",
            path.display(),
            metadata.len(),
            max_bytes
        )),
        Ok(_) => Ok(true),
    }
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("Failed to create {}: {error}", path.display()))?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), String> {
    fs::File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Failed to sync {}: {error}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn validate_prompt_payload(payload: &str) -> Result<(), String> {
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) > MAX_PROMPTS_BYTES {
        return Err(format!(
            "Prompt restore payload exceeds {MAX_PROMPTS_BYTES} bytes"
        ));
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct PromptPayload {
        version: u32,
        entries: Vec<crate::prompts::PromptEntry>,
        default_persona_id: Option<String>,
        has_seeded_defaults: bool,
    }
    let decoded: PromptPayload = serde_json::from_str(payload)
        .map_err(|error| format!("Invalid prompt restore payload: {error}"))?;
    if decoded.version != 1 || !decoded.has_seeded_defaults {
        return Err("Prompt restore payload must use initialized schema version 1".to_string());
    }
    if decoded.entries.len() > 100_000 {
        return Err("Prompt restore payload contains more than 100000 entries".to_string());
    }
    let mut ids = std::collections::HashSet::new();
    let mut commands = std::collections::HashSet::new();
    for entry in &decoded.entries {
        if entry.id.is_empty()
            || entry.id.len() > 256
            || entry.id.contains('\0')
            || !ids.insert(entry.id.clone())
        {
            return Err("Prompt restore payload contains an invalid or duplicate id".to_string());
        }
        if !matches!(entry.kind.as_str(), "persona" | "snippet")
            || entry.name.is_empty()
            || entry.name.len() > 16 * 1024
            || entry.command.len() > 32
            || entry.content.len() > 2 * 1024 * 1024
            || entry
                .description
                .as_ref()
                .is_some_and(|value| value.len() > 16 * 1024)
            || entry.created_at == 0
            || entry.updated_at == 0
            || !commands.insert(entry.command.clone())
        {
            return Err(format!("Prompt '{}' has invalid portable fields", entry.id));
        }
    }
    if decoded.default_persona_id.as_ref().is_some_and(|id| {
        !decoded
            .entries
            .iter()
            .any(|entry| entry.id == *id && entry.kind == "persona")
    }) {
        return Err("Default prompt persona does not reference an imported persona".to_string());
    }
    Ok(())
}

fn validate_bounded_json(value: &serde_json::Value) -> Result<(), String> {
    fn visit(value: &serde_json::Value, depth: usize, nodes: &mut usize) -> Result<(), String> {
        if depth > 32 {
            return Err("Portable settings exceed the JSON depth limit".to_string());
        }
        *nodes = nodes
            .checked_add(1)
            .ok_or_else(|| "Portable settings node-count overflow".to_string())?;
        if *nodes > 100_000 {
            return Err("Portable settings exceed the JSON node limit".to_string());
        }
        match value {
            serde_json::Value::String(value) if value.len() > 16 * 1024 || value.contains('\0') => {
                Err("Portable settings contain an oversized or NUL-bearing string".to_string())
            }
            serde_json::Value::Array(values) => {
                for child in values {
                    visit(child, depth + 1, nodes)?;
                }
                Ok(())
            }
            serde_json::Value::Object(values) => {
                for (key, child) in values {
                    if key.len() > 256 || key.contains('\0') {
                        return Err("Portable settings contain an invalid key".to_string());
                    }
                    visit(child, depth + 1, nodes)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    let mut nodes = 0;
    visit(value, 0, &mut nodes)
}

fn validated_pending_settings(
    transaction_id: &str,
    request: Option<PortableRestoreSettingsRequest>,
) -> Result<Option<PendingPortableRestoreSettings>, String> {
    let Some(request) = request else {
        return Ok(None);
    };
    if let Some(locale) = request.locale.as_deref() {
        if locale.len() < 2
            || locale.len() > 35
            || !locale.is_ascii()
            || locale.starts_with('-')
            || locale.ends_with('-')
            || locale
                .bytes()
                .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'-'))
        {
            return Err("Portable locale is not a bounded BCP-47-like tag".to_string());
        }
    }
    if let Some(overrides) = request.shortcut_overrides.as_ref() {
        if !overrides.is_object() {
            return Err("Portable shortcut overrides must be a JSON object".to_string());
        }
        validate_bounded_json(overrides)?;
    }
    if request.locale.is_none() && request.shortcut_overrides.is_none() {
        return Ok(None);
    }
    let pending = PendingPortableRestoreSettings {
        schema_version: 1,
        transaction_id: transaction_id.to_string(),
        locale: request.locale,
        shortcut_overrides: request.shortcut_overrides,
    };
    let size = serde_json::to_vec(&pending).map_err(command_error)?.len();
    if u64::try_from(size).unwrap_or(u64::MAX) > MAX_PENDING_SETTINGS_BYTES {
        return Err("Portable settings exceed their restore limit".to_string());
    }
    Ok(Some(pending))
}

fn prepare_stack_registry(
    base: &Path,
    transaction_root: &Path,
    incoming: Vec<crate::knowledge_core::KnowledgeStack>,
    replace: bool,
) -> Result<(Vec<crate::knowledge_core::KnowledgeStack>, Vec<u8>), String> {
    let planning_root = transaction_root.join("stack-plan");
    fs::create_dir_all(&planning_root)
        .map_err(|error| format!("Failed to create {}: {error}", planning_root.display()))?;
    let current = restore_target(base, RestoreFileKind::Stacks);
    if !replace && ensure_regular_or_missing(&current, MAX_STACK_REGISTRY_BYTES)? {
        fs::copy(&current, planning_root.join("index.json"))
            .map_err(|error| format!("Failed to stage {}: {error}", current.display()))?;
    }
    let stacks = crate::knowledge_core::import_definitions_impl(&planning_root, incoming, replace)?;
    let bytes = read_regular_bounded(&planning_root.join("index.json"), MAX_STACK_REGISTRY_BYTES)?;
    Ok((stacks, bytes))
}

fn read_restore_journal(transaction_root: &Path) -> Result<RestoreJournal, String> {
    let bytes = read_regular_bounded(
        &transaction_root.join(RESTORE_JOURNAL_FILE),
        MAX_CONFIG_BYTES,
    )?;
    let journal: RestoreJournal = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Portable restore journal is corrupt: {error}"))?;
    if journal.schema_version != 1
        || Uuid::parse_str(&journal.transaction_id).is_err()
        || journal.files.is_empty()
    {
        return Err("Portable restore journal has an invalid schema".to_string());
    }
    Ok(journal)
}

fn rollback_published_restore(
    base: &Path,
    transaction_root: &Path,
    journal: &RestoreJournal,
) -> Result<String, String> {
    let rollback_profile = String::from_utf8(read_regular_bounded(
        &transaction_root.join(RESTORE_ROLLBACK_PROFILE_FILE),
        crate::profile_store::MAX_PROFILE_JSON_BYTES,
    )?)
    .map_err(|_| "Portable rollback profile is not UTF-8".to_string())?;
    for entry in journal.files.iter().rev() {
        let target = restore_target(base, entry.kind);
        let backup = transaction_root
            .join("backup")
            .join(entry.kind.backup_name());
        let backup_exists = ensure_regular_or_missing(&backup, entry.kind.max_bytes())?;
        if backup_exists {
            if ensure_regular_or_missing(&target, entry.kind.max_bytes())? {
                fs::remove_file(&target).map_err(|error| {
                    format!(
                        "Failed to remove {} during rollback: {error}",
                        target.display()
                    )
                })?;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
            }
            fs::rename(&backup, &target).map_err(|error| {
                format!(
                    "Failed to restore {} from {}: {error}",
                    target.display(),
                    backup.display()
                )
            })?;
        } else if !entry.had_original && ensure_regular_or_missing(&target, entry.kind.max_bytes())?
        {
            fs::remove_file(&target).map_err(|error| {
                format!(
                    "Failed to remove {} during rollback: {error}",
                    target.display()
                )
            })?;
        }
        if let Some(parent) = target.parent() {
            sync_directory(parent)?;
        }
    }
    fs::remove_dir_all(transaction_root).map_err(|error| {
        format!(
            "Failed to remove recovered transaction {}: {error}",
            transaction_root.display()
        )
    })?;
    sync_directory(base)?;
    Ok(rollback_profile)
}

fn recover_restore_transactions_at(base: &Path) -> Result<Vec<String>, String> {
    let mut transaction_roots = Vec::new();
    for entry in
        fs::read_dir(base).map_err(|error| format!("Failed to scan {}: {error}", base.display()))?
    {
        let entry = entry.map_err(command_error)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(RESTORE_TRANSACTION_PREFIX) {
            continue;
        }
        let file_type = entry.file_type().map_err(command_error)?;
        if !file_type.is_dir() || file_type.is_symlink() {
            return Err(format!(
                "Portable restore transaction {} is not a regular directory",
                entry.path().display()
            ));
        }
        transaction_roots.push(entry.path());
    }
    transaction_roots.sort();
    let mut rollback_profiles = Vec::new();
    for transaction_root in transaction_roots {
        if transaction_root.join(RESTORE_COMMIT_MARKER).is_file() {
            fs::remove_dir_all(&transaction_root).map_err(|error| {
                format!("Failed to clean {}: {error}", transaction_root.display())
            })?;
            continue;
        }
        if !transaction_root.join(RESTORE_JOURNAL_FILE).exists() {
            // Preparation never reached the first live rename, so no rollback
            // is needed; the random, app-owned staging directory is disposable.
            fs::remove_dir_all(&transaction_root).map_err(|error| {
                format!("Failed to clean {}: {error}", transaction_root.display())
            })?;
            continue;
        }
        let journal = read_restore_journal(&transaction_root)?;
        rollback_profiles.push(rollback_published_restore(
            base,
            &transaction_root,
            &journal,
        )?);
    }
    Ok(rollback_profiles)
}

fn publish_restore_at(
    base: &Path,
    request: PortableRestoreRequest,
    fail_after_publish: Option<usize>,
) -> Result<(PublishedRestore, crate::profile_store::ProfileCounts, bool), String> {
    let profile_counts =
        crate::profile_store::validate_payload(&request.sessions_payload).map_err(command_error)?;
    crate::profile_store::validate_payload(&request.previous_sessions_payload)
        .map_err(|error| format!("Current profile cannot be used as a rollback point: {error}"))?;
    validate_prompt_payload(&request.prompts_payload)?;

    let transaction_id = Uuid::new_v4().to_string();
    let transaction_root = base.join(format!("{RESTORE_TRANSACTION_PREFIX}{transaction_id}"));
    fs::create_dir(&transaction_root)
        .map_err(|error| format!("Failed to create {}: {error}", transaction_root.display()))?;

    let prepared = (|| {
        let (stacks, stack_bytes) = prepare_stack_registry(
            base,
            &transaction_root,
            request.stacks,
            matches!(request.mode, PortableRestoreMode::Replace),
        )?;
        let pending_settings = validated_pending_settings(&transaction_id, request.settings)?;
        let settings_pending = pending_settings.is_some();
        let mut plans = vec![
            RestoreFilePlan {
                kind: RestoreFileKind::Sessions,
                bytes: request.sessions_payload.as_bytes().to_vec(),
            },
            RestoreFilePlan {
                kind: RestoreFileKind::Prompts,
                bytes: request.prompts_payload.into_bytes(),
            },
            RestoreFilePlan {
                kind: RestoreFileKind::Stacks,
                bytes: stack_bytes,
            },
        ];
        if let Some(settings) = pending_settings {
            plans.push(RestoreFilePlan {
                kind: RestoreFileKind::PendingSettings,
                bytes: serde_json::to_vec(&settings).map_err(command_error)?,
            });
        }

        let stage_root = transaction_root.join("stage");
        let backup_root = transaction_root.join("backup");
        fs::create_dir(&stage_root).map_err(command_error)?;
        fs::create_dir(&backup_root).map_err(command_error)?;
        write_new_synced(
            &transaction_root.join(RESTORE_ROLLBACK_PROFILE_FILE),
            request.previous_sessions_payload.as_bytes(),
        )?;

        let mut entries = Vec::with_capacity(plans.len());
        for plan in &plans {
            if u64::try_from(plan.bytes.len()).unwrap_or(u64::MAX) > plan.kind.max_bytes() {
                return Err(format!("{:?} restore payload exceeds its limit", plan.kind));
            }
            let target = restore_target(base, plan.kind);
            let had_original = ensure_regular_or_missing(&target, plan.kind.max_bytes())?;
            write_new_synced(&stage_root.join(plan.kind.stage_name()), &plan.bytes)?;
            entries.push(RestoreJournalEntry {
                kind: plan.kind,
                had_original,
            });
        }
        let journal = RestoreJournal {
            schema_version: 1,
            transaction_id: transaction_id.clone(),
            files: entries,
        };
        write_new_synced(
            &transaction_root.join(RESTORE_JOURNAL_FILE),
            &serde_json::to_vec(&journal).map_err(command_error)?,
        )?;
        sync_directory(&transaction_root)?;
        sync_directory(base)?;

        let publish = (|| {
            for (index, entry) in journal.files.iter().enumerate() {
                let target = restore_target(base, entry.kind);
                let parent = target
                    .parent()
                    .ok_or_else(|| "Portable restore target has no parent".to_string())?;
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
                if entry.had_original {
                    fs::rename(&target, backup_root.join(entry.kind.backup_name())).map_err(
                        |error| {
                            format!("Failed to stage {} for rollback: {error}", target.display())
                        },
                    )?;
                }
                fs::rename(stage_root.join(entry.kind.stage_name()), &target)
                    .map_err(|error| format!("Failed to publish {}: {error}", target.display()))?;
                sync_directory(parent)?;
                if fail_after_publish.is_some_and(|count| index + 1 >= count) {
                    return Err(format!(
                        "Injected portable restore failure after {} published files",
                        index + 1
                    ));
                }
            }
            Ok(())
        })();
        if let Err(error) = publish {
            return match rollback_published_restore(base, &transaction_root, &journal) {
                Ok(_) => Err(error),
                Err(rollback) => Err(format!("{error}; rollback also failed: {rollback}")),
            };
        }
        Ok((
            PublishedRestore {
                base: base.to_path_buf(),
                transaction_root: transaction_root.clone(),
                journal,
                stacks,
            },
            settings_pending,
        ))
    })();

    match prepared {
        Ok((published, settings_pending)) => Ok((published, profile_counts, settings_pending)),
        Err(error) => {
            if transaction_root.exists() && !transaction_root.join(RESTORE_JOURNAL_FILE).exists() {
                let _ = fs::remove_dir_all(&transaction_root);
            }
            Err(error)
        }
    }
}

impl PublishedRestore {
    fn rollback(&self) -> Result<String, String> {
        rollback_published_restore(&self.base, &self.transaction_root, &self.journal)
    }

    fn mark_committed(&self) -> Result<(), String> {
        write_new_synced(
            &self.transaction_root.join(RESTORE_COMMIT_MARKER),
            self.journal.transaction_id.as_bytes(),
        )?;
        sync_directory(&self.transaction_root)
    }

    fn cleanup(&self) {
        let _ = fs::remove_dir_all(&self.transaction_root);
        let _ = sync_directory(&self.base);
    }
}

fn recover_pending_portable_restores_locked(
    app: &tauri::AppHandle,
    state: &crate::AppState,
) -> Result<(), String> {
    for rollback_payload in recover_restore_transactions_at(&app_data_root(app)?)? {
        // The durable file set has already been rolled back. Re-normalize the
        // same previous snapshot into the search/profile database so a crash
        // in the narrow post-SQLite/pre-commit-marker window also converges.
        crate::profile_commands::sync_profile_payload(app, state, &rollback_payload)?;
    }
    Ok(())
}

pub(crate) fn recover_pending_portable_restores(
    app: &tauri::AppHandle,
    state: &crate::AppState,
) -> Result<(), String> {
    let _guard = restore_lock()
        .lock()
        .map_err(|_| "Portable restore lock poisoned".to_string())?;
    recover_pending_portable_restores_locked(app, state)
}

#[tauri::command]
pub fn portable_restore_apply(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, crate::AppState>,
    request: PortableRestoreRequest,
) -> Result<PortableRestoreOutcome, String> {
    let _guard = restore_lock()
        .lock()
        .map_err(|_| "Portable restore lock poisoned".to_string())?;
    recover_pending_portable_restores_locked(&app, state.inner())?;
    let (published, profile_counts, settings_pending) =
        publish_restore_at(&app_data_root(&app)?, request, None)?;

    let sessions_payload =
        fs::read_to_string(restore_target(&published.base, RestoreFileKind::Sessions))
            .map_err(command_error)?;
    if let Err(error) =
        crate::profile_commands::sync_profile_payload(&app, state.inner(), &sessions_payload)
    {
        let rollback = published.rollback();
        return Err(match rollback {
            Ok(_) => error,
            Err(rollback) => format!("{error}; rollback also failed: {rollback}"),
        });
    }
    if let Err(error) = published.mark_committed() {
        let rollback_payload = published.rollback();
        return match rollback_payload {
            Ok(payload) => {
                let profile_rollback =
                    crate::profile_commands::sync_profile_payload(&app, state.inner(), &payload);
                Err(match profile_rollback {
                    Ok(_) => error,
                    Err(rollback) => {
                        format!("{error}; profile rollback also failed: {rollback}")
                    }
                })
            }
            Err(rollback) => Err(format!("{error}; file rollback also failed: {rollback}")),
        };
    }
    let outcome = PortableRestoreOutcome {
        transaction_id: published.journal.transaction_id.clone(),
        stacks: published.stacks.clone(),
        profile_counts,
        settings_pending,
    };
    published.cleanup();
    let _ = app.emit(crate::sessions::SESSIONS_CHANGED_EVENT, window.label());
    let _ = app.emit(crate::prompts::PROMPTS_CHANGED_EVENT, window.label());
    Ok(outcome)
}

#[tauri::command]
pub fn portable_restore_settings_pending(
    app: tauri::AppHandle,
) -> Result<Option<PendingPortableRestoreSettings>, String> {
    let _guard = restore_lock()
        .lock()
        .map_err(|_| "Portable restore lock poisoned".to_string())?;
    let path = app_data_root(&app)?.join(PENDING_RESTORE_SETTINGS_FILE);
    if !ensure_regular_or_missing(&path, MAX_PENDING_SETTINGS_BYTES)? {
        return Ok(None);
    }
    let pending: PendingPortableRestoreSettings =
        serde_json::from_slice(&read_regular_bounded(&path, MAX_PENDING_SETTINGS_BYTES)?)
            .map_err(|error| format!("Pending portable settings are invalid: {error}"))?;
    if pending.schema_version != 1 || Uuid::parse_str(&pending.transaction_id).is_err() {
        return Err("Pending portable settings have an invalid schema".to_string());
    }
    validated_pending_settings(
        &pending.transaction_id,
        Some(PortableRestoreSettingsRequest {
            locale: pending.locale.clone(),
            shortcut_overrides: pending.shortcut_overrides.clone(),
        }),
    )?;
    Ok(Some(pending))
}

#[tauri::command]
pub fn portable_restore_settings_acknowledge(
    app: tauri::AppHandle,
    transaction_id: String,
) -> Result<bool, String> {
    Uuid::parse_str(&transaction_id)
        .map_err(|_| "Portable restore transaction id is invalid".to_string())?;
    let _guard = restore_lock()
        .lock()
        .map_err(|_| "Portable restore lock poisoned".to_string())?;
    let base = app_data_root(&app)?;
    let path = base.join(PENDING_RESTORE_SETTINGS_FILE);
    if !ensure_regular_or_missing(&path, MAX_PENDING_SETTINGS_BYTES)? {
        return Ok(false);
    }
    let pending: PendingPortableRestoreSettings =
        serde_json::from_slice(&read_regular_bounded(&path, MAX_PENDING_SETTINGS_BYTES)?)
            .map_err(|error| format!("Pending portable settings are invalid: {error}"))?;
    if pending.transaction_id != transaction_id {
        return Ok(false);
    }
    fs::remove_file(&path)
        .map_err(|error| format!("Failed to acknowledge {}: {error}", path.display()))?;
    sync_directory(&base)?;
    Ok(true)
}

#[tauri::command]
pub fn portable_export_bundle(
    path: String,
    request: PortableBundleRequest,
) -> Result<ImportPreflightReport, String> {
    let bytes = export_portable_bundle(&request.into_core()?).map_err(command_error)?;
    let (_, report) =
        preflight_portable_bundle(&bytes, &ImportLimits::default()).map_err(command_error)?;
    atomic_write(Path::new(&path), &bytes)?;
    Ok(report)
}

#[tauri::command]
pub fn portable_read_bundle(path: String) -> Result<PortableReadOutcome, String> {
    read_outcome(&read_bundle_bytes(Path::new(&path))?)
}

#[tauri::command]
pub fn portable_export_session(
    path: String,
    format: String,
    session: PortableSession,
) -> Result<(), String> {
    let bytes = match format.as_str() {
        "markdown" => export_session_markdown(&session)
            .map_err(command_error)?
            .into_bytes(),
        "json" => serde_json::to_vec_pretty(&session).map_err(command_error)?,
        "docx" => export_session_docx(&session).map_err(command_error)?,
        _ => {
            return Err(
                "Unsupported session export format; choose markdown, json, or docx".to_string(),
            )
        }
    };
    atomic_write(Path::new(&path), &bytes)
}

struct KeychainAes256Gcm {
    key: LessSafeKey,
    key_id: String,
}

impl KeychainAes256Gcm {
    fn load_or_create() -> Result<Self, String> {
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, BACKUP_KEY_ACCOUNT)
            .map_err(|error| format!("Failed to access backup keychain entry: {error}"))?;
        let key_bytes = match entry.get_password() {
            Ok(encoded) => base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|error| format!("Backup keychain entry is corrupt: {error}"))?,
            Err(keyring::Error::NoEntry) => {
                let mut generated = vec![0_u8; 32];
                SystemRandom::new()
                    .fill(&mut generated)
                    .map_err(|_| "The operating system random generator failed".to_string())?;
                entry
                    .set_password(&base64::engine::general_purpose::STANDARD.encode(&generated))
                    .map_err(|error| {
                        format!("Failed to store the backup key in the keychain: {error}")
                    })?;
                generated
            }
            Err(error) => return Err(format!("Failed to read the backup keychain entry: {error}")),
        };
        if key_bytes.len() != AES_256_GCM.key_len() {
            return Err("Backup keychain entry has the wrong key length".to_string());
        }
        let key_id = format!("backup-{}", &sha256_hex(&key_bytes)[..24]);
        let unbound = UnboundKey::new(&AES_256_GCM, &key_bytes)
            .map_err(|_| "Backup keychain entry is not a valid AES-256 key".to_string())?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
            key_id,
        })
    }
}

impl CryptoProvider for KeychainAes256Gcm {
    fn algorithm_id(&self) -> &str {
        "AES-256-GCM-ring-0.17"
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<CryptoSeal, String> {
        let mut nonce = [0_u8; 12];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| "The operating system random generator failed".to_string())?;
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .map_err(|_| "Authenticated snapshot encryption failed".to_string())?;
        let tag = in_out.split_off(in_out.len().checked_sub(AES_256_GCM.tag_len()).ok_or_else(
            || "Encrypted snapshot is shorter than its authentication tag".to_string(),
        )?);
        Ok(CryptoSeal {
            nonce: nonce.to_vec(),
            ciphertext: in_out,
            tag,
        })
    }

    fn open(
        &self,
        aad: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, String> {
        let nonce: [u8; 12] = nonce
            .try_into()
            .map_err(|_| "Encrypted snapshot nonce must be exactly 96 bits".to_string())?;
        if tag.len() != AES_256_GCM.tag_len() {
            return Err("Encrypted snapshot authentication tag has the wrong length".to_string());
        }
        let mut in_out = Vec::with_capacity(ciphertext.len() + tag.len());
        in_out.extend_from_slice(ciphertext);
        in_out.extend_from_slice(tag);
        let plaintext = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .map_err(|_| {
                "Snapshot authentication failed; the key or bytes do not match".to_string()
            })?;
        Ok(plaintext.to_vec())
    }
}

fn snapshot_root(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|path| snapshot_root_at(&path))
        .map_err(command_error)
}

fn snapshot_root_at(app_data: &Path) -> PathBuf {
    app_data.join(SNAPSHOT_DIRECTORY)
}

fn current_source_revision(app: &tauri::AppHandle) -> Result<String, String> {
    let path = crate::sessions::sessions_file_path(app)?;
    current_source_revision_path(&path)
}

fn current_source_revision_path(path: &Path) -> Result<String, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(sha256_hex(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(sha256_hex(b"empty-profile"))
        }
        Err(error) => Err(format!(
            "Failed to read the current profile revision: {error}"
        )),
    }
}

#[tauri::command]
pub fn portable_snapshot_create(
    app: tauri::AppHandle,
    request: PortableBundleRequest,
    retention: Option<SnapshotRetentionPolicy>,
) -> Result<SnapshotWriteOutcome, String> {
    let archive = export_portable_bundle(&request.into_core()?).map_err(command_error)?;
    let crypto = KeychainAes256Gcm::load_or_create()?;
    let created_at_ms = now_ms()?;
    let envelope = seal_encrypted_snapshot(
        &archive,
        &current_source_revision(&app)?,
        created_at_ms,
        &ImportLimits::default(),
        &crypto,
    )
    .map_err(command_error)?;
    write_snapshot_with_retention(
        snapshot_root(&app)?,
        &envelope,
        &retention.unwrap_or_default(),
        created_at_ms,
    )
    .map_err(command_error)
}

#[tauri::command]
pub fn portable_snapshot_list(app: tauri::AppHandle) -> Result<Vec<SnapshotFileInfo>, String> {
    list_snapshot_files(snapshot_root(&app)?).map_err(command_error)
}

fn open_snapshot_path(path: &Path) -> Result<PortableReadOutcome, String> {
    let max = ImportLimits::default().max_archive_bytes.saturating_mul(2);
    let bytes = read_regular_bounded(path, max)?;
    let crypto = KeychainAes256Gcm::load_or_create()?;
    let opened = open_encrypted_snapshot(&bytes, &ImportLimits::default(), &crypto)
        .map_err(command_error)?;
    let media_types = opened
        .bundle
        .manifest
        .artifacts
        .iter()
        .map(|descriptor| (descriptor.id.clone(), descriptor.media_type.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let artifacts = opened
        .bundle
        .artifacts
        .into_iter()
        .map(|(id, bytes)| PortableArtifactResponse {
            media_type: media_types
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            id,
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect();
    Ok(PortableReadOutcome {
        data: opened.bundle.data,
        artifacts,
        preflight: opened.preflight,
    })
}

#[tauri::command]
pub fn portable_snapshot_open(path: String) -> Result<PortableReadOutcome, String> {
    open_snapshot_path(Path::new(&path))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct WebDavBackupConfig {
    pub enabled: bool,
    pub base_url: String,
    pub username: String,
    pub remote_path: String,
    pub device_id: String,
    pub interval_minutes: u64,
    pub known_etag: Option<String>,
    pub last_attempt_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
    pub next_due_ms: Option<u64>,
    pub last_uploaded_sha256: Option<String>,
    pub last_uploaded_remote_path: Option<String>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl Default for WebDavBackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            username: String::new(),
            remote_path: "LittleMonkey/latest.lmsnapshot".to_string(),
            device_id: format!("device-{}", Uuid::new_v4()),
            interval_minutes: 24 * 60,
            known_etag: None,
            last_attempt_ms: None,
            last_success_ms: None,
            next_due_ms: None,
            last_uploaded_sha256: None,
            last_uploaded_remote_path: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveWebDavConfigRequest {
    pub enabled: bool,
    pub base_url: String,
    pub username: String,
    pub password: Option<String>,
    pub remote_path: String,
    pub interval_minutes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavStagedSnapshot {
    pub path: PathBuf,
    pub created_at_ms: u64,
    pub byte_size: u64,
    pub sha256: String,
    pub source_revision_sha256: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDavBackupStatus {
    pub config: WebDavBackupConfig,
    pub staged_snapshot: Option<WebDavStagedSnapshot>,
    pub credentials_available: bool,
    pub upload_claimed: bool,
    pub claim_owner: Option<String>,
    pub claim_expires_ms: Option<u64>,
    pub ready: bool,
    pub readiness_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebDavBackgroundRunOutcome {
    Disabled,
    NotDue {
        next_due_ms: u64,
    },
    MissingStagedSource,
    Busy {
        owner: String,
        expires_at_ms: u64,
    },
    AlreadyCurrent {
        snapshot_sha256: String,
        next_due_ms: u64,
    },
    Uploaded {
        remote_path: String,
        etag: String,
        snapshot_sha256: String,
    },
    ConflictCopy {
        remote_path: String,
        etag: String,
        conflicting_etag: Option<String>,
        snapshot_sha256: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WebDavUploadAttempt {
    schema_version: u32,
    attempt_id: String,
    created_at_ms: u64,
    snapshot_sha256: String,
    base_url: String,
    username: String,
    remote_path: String,
    device_id: String,
    expected_etag: Option<String>,
    dispatch_started: bool,
}

#[derive(Clone, Debug)]
struct BackupClaimInfo {
    owner: String,
    expires_at_ms: u64,
}

struct BackupClaimGuard {
    database: PathBuf,
    lease_id: String,
    released: bool,
}

enum BackupClaimOutcome {
    Acquired(BackupClaimGuard),
    Busy(BackupClaimInfo),
}

fn config_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|path| config_path_at(&path))
        .map_err(command_error)
}

fn config_path_at(app_data: &Path) -> PathBuf {
    app_data.join(BACKUP_CONFIG_FILE)
}

fn validate_remote_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 2_048
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.contains('?')
        || path.contains('#')
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err("WebDAV remote path is unsafe or malformed".to_string());
    }
    Ok(())
}

fn normalized_webdav_base(value: &str) -> Result<String, String> {
    let mut url =
        Url::parse(value.trim()).map_err(|error| format!("Invalid WebDAV URL: {error}"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("WebDAV URL must not contain credentials, a query, or a fragment".to_string());
    }
    let loopback = match url.host() {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(
            "WebDAV requires HTTPS; plain HTTP is allowed only for loopback testing".to_string(),
        );
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url.to_string())
}

fn validate_etag(etag: &str) -> Result<(), String> {
    if etag.is_empty() || etag.len() > 1_024 || etag.contains(['\r', '\n', '\0']) {
        return Err("WebDAV returned an unsafe ETag".to_string());
    }
    Ok(())
}

fn load_webdav_config(app: &tauri::AppHandle) -> Result<WebDavBackupConfig, String> {
    load_webdav_config_path(&config_path(app)?)
}

fn load_webdav_config_at(app_data: &Path) -> Result<WebDavBackupConfig, String> {
    load_webdav_config_path(&config_path_at(app_data))
}

fn load_webdav_config_path(path: &Path) -> Result<WebDavBackupConfig, String> {
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(WebDavBackupConfig::default())
        }
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(metadata) if metadata.len() > MAX_CONFIG_BYTES => {
            Err("Backup configuration exceeds its size limit".to_string())
        }
        Ok(_) => {
            let bytes = fs::read(&path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
            let config: WebDavBackupConfig = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Backup configuration is invalid: {error}"))?;
            validate_stored_config(config)
        }
    }
}

fn validate_stored_config(mut config: WebDavBackupConfig) -> Result<WebDavBackupConfig, String> {
    if config.base_url.is_empty() {
        if config.enabled {
            return Err("Enabled WebDAV backup is missing its server URL".to_string());
        }
    } else {
        config.base_url = normalized_webdav_base(&config.base_url)?;
    }
    validate_remote_path(&config.remote_path)?;
    if config.username.len() > 512 {
        return Err("WebDAV username exceeds 512 characters".to_string());
    }
    if !(5..=7 * 24 * 60).contains(&config.interval_minutes) {
        return Err("Backup interval must be between 5 minutes and 7 days".to_string());
    }
    if config.device_id.is_empty() || config.device_id.len() > 128 {
        return Err("Backup device id is invalid".to_string());
    }
    if let Some(etag) = config.known_etag.as_deref() {
        validate_etag(etag)?;
    }
    if let Some(digest) = config.last_uploaded_sha256.as_deref() {
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("Last uploaded snapshot digest is invalid".to_string());
        }
    }
    if let Some(path) = config.last_uploaded_remote_path.as_deref() {
        validate_remote_path(path)?;
    }
    if config
        .last_error
        .as_ref()
        .is_some_and(|message| message.len() > 4_096 || message.contains('\0'))
    {
        return Err("Last WebDAV error exceeds its safety limit".to_string());
    }
    Ok(config)
}

fn save_config_file(app: &tauri::AppHandle, config: &WebDavBackupConfig) -> Result<(), String> {
    save_config_file_at_path(&config_path(app)?, config)
}

fn save_config_file_at(app_data: &Path, config: &WebDavBackupConfig) -> Result<(), String> {
    save_config_file_at_path(&config_path_at(app_data), config)
}

fn save_config_file_at_path(path: &Path, config: &WebDavBackupConfig) -> Result<(), String> {
    let config = validate_stored_config(config.clone())?;
    let parent = path
        .parent()
        .ok_or_else(|| "Backup config has no parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec(&config).map_err(command_error)?;
    atomic_write(path, &bytes)
}

fn ensure_private_backup_directory(app_data: &Path) -> Result<PathBuf, String> {
    let root = snapshot_root_at(app_data);
    fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create {}: {error}", root.display()))?;
    let metadata = fs::symlink_metadata(&root)
        .map_err(|error| format!("Failed to inspect {}: {error}", root.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Failed to protect {}: {error}", root.display()))?;
    }
    Ok(root)
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    atomic_write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to protect {}: {error}", path.display()))?;
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Private file has no parent directory".to_string())?;
    sync_directory(parent)
}

fn staged_snapshot_path(app_data: &Path) -> PathBuf {
    snapshot_root_at(app_data).join(STAGED_SNAPSHOT_FILE)
}

fn staged_snapshot_info_from_bytes(
    path: PathBuf,
    bytes: &[u8],
) -> Result<WebDavStagedSnapshot, String> {
    let envelope: EncryptedSnapshotEnvelopeV1 = serde_json::from_slice(bytes)
        .map_err(|error| format!("Staged snapshot envelope is invalid: {error}"))?;
    Ok(WebDavStagedSnapshot {
        path,
        created_at_ms: envelope.created_at_ms,
        byte_size: u64::try_from(bytes.len()).map_err(command_error)?,
        sha256: sha256_hex(bytes),
        source_revision_sha256: envelope.source_revision_sha256,
    })
}

fn read_staged_snapshot(
    app_data: &Path,
) -> Result<Option<(WebDavStagedSnapshot, Vec<u8>)>, String> {
    let path = staged_snapshot_path(app_data);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(_) => {
            let bytes = read_regular_bounded(
                &path,
                ImportLimits::default().max_archive_bytes.saturating_mul(2),
            )?;
            let info = staged_snapshot_info_from_bytes(path, &bytes)?;
            Ok(Some((info, bytes)))
        }
    }
}

fn scheduler_database_path(app_data: &Path) -> PathBuf {
    app_data.join(BACKUP_SCHEDULER_DB)
}

fn open_scheduler_database(path: &Path) -> Result<Connection, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Backup scheduler database has no parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!("{} is not a regular file", path.display()));
        }
    }
    let connection = Connection::open(path)
        .map_err(|error| format!("Failed to open {}: {error}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(command_error)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS webdav_upload_claim (
               singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
               lease_id TEXT NOT NULL,
               owner TEXT NOT NULL,
               acquired_at_ms INTEGER NOT NULL,
               expires_at_ms INTEGER NOT NULL
             );",
        )
        .map_err(command_error)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Failed to protect {}: {error}", path.display()))?;
    }
    Ok(connection)
}

fn acquire_backup_claim(
    app_data: &Path,
    owner: &str,
    now: u64,
) -> Result<BackupClaimOutcome, String> {
    if owner.is_empty()
        || owner.len() > 256
        || owner.chars().any(|character| character.is_control())
    {
        return Err("Backup scheduler owner id is invalid".to_string());
    }
    let database = scheduler_database_path(app_data);
    let mut connection = open_scheduler_database(&database)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(command_error)?;
    let existing = transaction
        .query_row(
            "SELECT owner, expires_at_ms FROM webdav_upload_claim WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(command_error)?;
    if let Some((existing_owner, expires_at_ms)) = existing {
        let expires_at_ms = u64::try_from(expires_at_ms).unwrap_or_default();
        if expires_at_ms > now {
            transaction.commit().map_err(command_error)?;
            return Ok(BackupClaimOutcome::Busy(BackupClaimInfo {
                owner: existing_owner,
                expires_at_ms,
            }));
        }
    }
    let lease_id = Uuid::new_v4().to_string();
    let expires_at_ms = now.saturating_add(BACKUP_CLAIM_TTL_MS);
    transaction
        .execute(
            "INSERT INTO webdav_upload_claim(singleton, lease_id, owner, acquired_at_ms, expires_at_ms)
             VALUES(1, ?1, ?2, ?3, ?4)
             ON CONFLICT(singleton) DO UPDATE SET
               lease_id = excluded.lease_id,
               owner = excluded.owner,
               acquired_at_ms = excluded.acquired_at_ms,
               expires_at_ms = excluded.expires_at_ms",
            params![
                lease_id,
                owner,
                i64::try_from(now).map_err(command_error)?,
                i64::try_from(expires_at_ms).map_err(command_error)?,
            ],
        )
        .map_err(command_error)?;
    transaction.commit().map_err(command_error)?;
    Ok(BackupClaimOutcome::Acquired(BackupClaimGuard {
        database,
        lease_id,
        released: false,
    }))
}

fn current_backup_claim(app_data: &Path, now: u64) -> Result<Option<BackupClaimInfo>, String> {
    let database = scheduler_database_path(app_data);
    if !database.exists() {
        return Ok(None);
    }
    let connection = open_scheduler_database(&database)?;
    connection
        .query_row(
            "SELECT owner, expires_at_ms FROM webdav_upload_claim
             WHERE singleton = 1 AND expires_at_ms > ?1",
            params![i64::try_from(now).map_err(command_error)?],
            |row| {
                Ok(BackupClaimInfo {
                    owner: row.get(0)?,
                    expires_at_ms: u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                })
            },
        )
        .optional()
        .map_err(command_error)
}

impl BackupClaimGuard {
    fn release(&mut self) -> Result<(), String> {
        if self.released {
            return Ok(());
        }
        let connection = open_scheduler_database(&self.database)?;
        connection
            .execute(
                "DELETE FROM webdav_upload_claim WHERE singleton = 1 AND lease_id = ?1",
                params![self.lease_id],
            )
            .map_err(command_error)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for BackupClaimGuard {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn credential_account(config: &WebDavBackupConfig) -> String {
    let digest = sha256_hex(format!("{}\0{}", config.base_url, config.username).as_bytes());
    format!("webdav-{}", &digest[..32])
}

fn load_webdav_password(config: &WebDavBackupConfig) -> Result<String, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &credential_account(config))
        .map_err(|error| format!("Failed to access WebDAV keychain entry: {error}"))?
        .get_password()
        .map_err(|error| format!("Failed to read WebDAV password from keychain: {error}"))
}

pub fn stage_webdav_snapshot_at(
    app_data: &Path,
    request: PortableBundleRequest,
) -> Result<WebDavStagedSnapshot, String> {
    // Exclude volatile bundle id/export time so reopening the desktop with an
    // unchanged frontend profile reuses the prior encrypted source instead of
    // manufacturing another remote version. All user-owned data and artifact
    // bytes remain part of the revision digest.
    let source_revision_sha256 = sha256_hex(
        &serde_json::to_vec(&serde_json::json!({
            "appVersion": &request.app_version,
            "data": &request.data,
            "artifacts": &request.artifacts,
        }))
        .map_err(command_error)?,
    );
    let crypto = KeychainAes256Gcm::load_or_create()?;
    if let Some((staged, bytes)) = read_staged_snapshot(app_data)? {
        if staged.source_revision_sha256 == source_revision_sha256
            && open_encrypted_snapshot(&bytes, &ImportLimits::default(), &crypto).is_ok()
        {
            return Ok(staged);
        }
    }
    let archive = export_portable_bundle(&request.into_core()?).map_err(command_error)?;
    let created_at_ms = now_ms()?;
    let envelope = seal_encrypted_snapshot(
        &archive,
        &source_revision_sha256,
        created_at_ms,
        &ImportLimits::default(),
        &crypto,
    )
    .map_err(command_error)?;
    let root = ensure_private_backup_directory(app_data)?;
    let path = root.join(STAGED_SNAPSHOT_FILE);
    atomic_write_private(&path, &envelope)?;
    staged_snapshot_info_from_bytes(path, &envelope)
}

#[tauri::command]
pub fn portable_snapshot_stage_source(
    app: tauri::AppHandle,
    request: PortableBundleRequest,
) -> Result<WebDavStagedSnapshot, String> {
    let app_data = app.path().app_data_dir().map_err(command_error)?;
    stage_webdav_snapshot_at(&app_data, request)
}

pub fn webdav_backup_status_at(app_data: &Path) -> Result<WebDavBackupStatus, String> {
    let config = load_webdav_config_at(app_data)?;
    let staged_snapshot = read_staged_snapshot(app_data)?.map(|(info, _)| info);
    let credential_result = if config.enabled {
        load_webdav_password(&config).and_then(|_| KeychainAes256Gcm::load_or_create().map(|_| ()))
    } else {
        Ok(())
    };
    let claim = current_backup_claim(app_data, now_ms()?)?;
    let readiness_error = if !config.enabled {
        Some("Scheduled WebDAV backup is disabled".to_string())
    } else if staged_snapshot.is_none() {
        Some("No encrypted frontend snapshot has been staged yet".to_string())
    } else if let Err(error) = &credential_result {
        Some(error.clone())
    } else {
        None
    };
    Ok(WebDavBackupStatus {
        ready: readiness_error.is_none(),
        readiness_error,
        credentials_available: credential_result.is_ok(),
        upload_claimed: claim.is_some(),
        claim_owner: claim.as_ref().map(|claim| claim.owner.clone()),
        claim_expires_ms: claim.as_ref().map(|claim| claim.expires_at_ms),
        config,
        staged_snapshot,
    })
}

#[tauri::command]
pub fn portable_webdav_status_get(app: tauri::AppHandle) -> Result<WebDavBackupStatus, String> {
    let app_data = app.path().app_data_dir().map_err(command_error)?;
    webdav_backup_status_at(&app_data)
}

#[tauri::command]
pub fn portable_webdav_config_save(
    app: tauri::AppHandle,
    request: SaveWebDavConfigRequest,
) -> Result<WebDavBackupConfig, String> {
    let app_data = app.path().app_data_dir().map_err(command_error)?;
    let mut claim = match acquire_backup_claim(
        &app_data,
        &format!("desktop-config-{}", Uuid::new_v4()),
        now_ms()?,
    )? {
        BackupClaimOutcome::Busy(info) => {
            return Err(format!(
                "WebDAV backup is active in {} until {}; retry configuration after it finishes",
                info.owner, info.expires_at_ms
            ))
        }
        BackupClaimOutcome::Acquired(claim) => claim,
    };
    let previous = load_webdav_config(&app).unwrap_or_default();
    let now = now_ms()?;
    let base_url = normalized_webdav_base(&request.base_url)?;
    let username = request.username.trim().to_string();
    let remote_path = request.remote_path.trim().to_string();
    let same_target = previous.base_url == base_url
        && previous.username == username
        && previous.remote_path == remote_path;
    if !same_target && load_upload_attempt(&app_data)?.is_some() {
        claim.release()?;
        return Err(
            "A crash-recovery WebDAV upload is pending; let the daemon reconcile it before changing the remote target"
                .to_string(),
        );
    }
    let mut config = WebDavBackupConfig {
        enabled: request.enabled,
        base_url,
        username,
        remote_path,
        device_id: previous.device_id,
        interval_minutes: request.interval_minutes,
        known_etag: if same_target {
            previous.known_etag
        } else {
            None
        },
        last_attempt_ms: previous.last_attempt_ms,
        last_success_ms: previous.last_success_ms,
        next_due_ms: request
            .enabled
            .then_some(now.saturating_add(request.interval_minutes.saturating_mul(60_000))),
        last_uploaded_sha256: same_target
            .then_some(previous.last_uploaded_sha256)
            .flatten(),
        last_uploaded_remote_path: same_target
            .then_some(previous.last_uploaded_remote_path)
            .flatten(),
        last_error: None,
        consecutive_failures: 0,
    };
    config = validate_stored_config(config)?;
    if let Some(password) = request.password {
        if password.is_empty() || password.len() > 16_384 {
            return Err("WebDAV password must be 1..=16384 characters".to_string());
        }
        keyring::Entry::new(KEYCHAIN_SERVICE, &credential_account(&config))
            .map_err(|error| format!("Failed to access WebDAV keychain entry: {error}"))?
            .set_password(&password)
            .map_err(|error| format!("Failed to save WebDAV password in the keychain: {error}"))?;
    } else if config.enabled {
        let _ = load_webdav_password(&config)?;
    }
    save_config_file(&app, &config)?;
    claim.release()?;
    Ok(config)
}

/// The client every WebDAV operation shares.
///
/// [`WEB_DAV_TIMEOUT_SECONDS`] is a **silence** budget here, not a deadline for
/// the whole request, and the difference is the difference between a working
/// backup and one that cannot succeed. `reqwest::ClientBuilder::timeout` covers
/// the body too, so pairing it with the streaming download in
/// [`portable_webdav_download_snapshot`] meant a snapshot had 45 seconds to
/// arrive in full — against a cap of `2 × max_archive_bytes`, i.e. 1 GiB, which
/// needs 23 MB/s sustained for the entire request. Any backup past a couple of
/// hundred megabytes was aborted mid-stream on an ordinary connection, and the
/// upload half had the same ceiling.
///
/// `read_timeout` is the right shape because it resets on every read: a peer
/// that stops sending for 45 seconds is still declared dead, while one that
/// keeps making progress is allowed to take as long as the transfer honestly
/// takes. The bound on *size* stays where it already was — the `Content-Length`
/// pre-check and the running total in the download loop.
///
/// Built on [`crate::egress::hardened_with_read_budget`] for the connect timeout
/// and the budget, then overriding its redirect policy back to `none`: a WebDAV
/// server has no business redirecting, [`remote_url`] pins every path to the
/// configured origin, and refusing outright is stricter than the same-origin
/// rule `hardened` supplies.
///
/// # What this does not bound
///
/// A server that accepts a connection and then stops *reading* during an upload.
/// reqwest has no write timeout, so the old total deadline did cover that case
/// and this does not. Accepted deliberately: it needs a pathological peer,
/// whereas the truncation it replaces broke every large backup against a
/// perfectly healthy one.
fn webdav_client() -> Result<reqwest::Client, String> {
    crate::egress::hardened_with_read_budget(Duration::from_secs(WEB_DAV_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("LittleMonkey/0.1 WebDAVBackup")
        .build()
        .map_err(command_error)
}

fn remote_url(config: &WebDavBackupConfig, remote_path: &str) -> Result<Url, String> {
    validate_remote_path(remote_path)?;
    let base = Url::parse(&config.base_url).map_err(command_error)?;
    let joined = base.join(remote_path).map_err(command_error)?;
    if joined.origin() != base.origin() {
        return Err("WebDAV remote path escaped the configured origin".to_string());
    }
    Ok(joined)
}

async fn put_webdav(
    client: &reqwest::Client,
    config: &WebDavBackupConfig,
    password: &str,
    remote_path: &str,
    bytes: &[u8],
    condition: Option<&str>,
) -> Result<(reqwest::StatusCode, Option<String>), String> {
    let mut request = client
        .put(remote_url(config, remote_path)?)
        .basic_auth(&config.username, Some(password))
        .header(
            "Content-Type",
            "application/vnd.little-monkey.encrypted-snapshot",
        )
        .body(bytes.to_vec());
    request = match condition {
        Some(etag) => request.header(IF_MATCH, etag),
        None => request.header(IF_NONE_MATCH, "*"),
    };
    let response = crate::egress::send(request).await.map_err(command_error)?;
    if response.status().is_redirection() {
        return Err(
            "WebDAV redirects are disabled to keep credentials on the configured origin"
                .to_string(),
        );
    }
    let etag = response
        .headers()
        .get(ETAG)
        .map(|value| value.to_str().map(str::to_string).map_err(command_error))
        .transpose()?;
    if let Some(etag) = etag.as_deref() {
        validate_etag(etag)?;
    }
    Ok((response.status(), etag))
}

fn conflict_remote_path(
    remote_path: &str,
    device_id: &str,
    timestamp: u64,
    bytes: &[u8],
) -> Result<String, String> {
    validate_remote_path(remote_path)?;
    let file_start = remote_path.rfind('/').map_or(0, |slash| slash + 1);
    let split = remote_path[file_start..]
        .rfind('.')
        .filter(|dot| *dot > 0 && file_start + dot + 1 < remote_path.len())
        .map(|dot| file_start + dot);
    let (stem, extension) = split.map_or((remote_path, ""), |dot| {
        (&remote_path[..dot], &remote_path[dot..])
    });
    let device_hash = sha256_hex(device_id.as_bytes());
    let bytes_hash = sha256_hex(bytes);
    let path = format!(
        "{stem}.conflict-{}-{timestamp}-{}{}",
        &device_hash[..12],
        &bytes_hash[..12],
        extension
    );
    validate_remote_path(&path)?;
    Ok(path)
}

fn attempt_journal_path(app_data: &Path) -> PathBuf {
    snapshot_root_at(app_data).join(ATTEMPT_JOURNAL_FILE)
}

fn attempt_snapshot_path(app_data: &Path) -> PathBuf {
    snapshot_root_at(app_data).join(ATTEMPT_SNAPSHOT_FILE)
}

fn validate_upload_attempt(attempt: WebDavUploadAttempt) -> Result<WebDavUploadAttempt, String> {
    if attempt.schema_version != 1
        || Uuid::parse_str(&attempt.attempt_id).is_err()
        || attempt.created_at_ms == 0
        || attempt.snapshot_sha256.len() != 64
        || !attempt
            .snapshot_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("WebDAV upload intent identity is invalid".to_string());
    }
    if normalized_webdav_base(&attempt.base_url)? != attempt.base_url {
        return Err("WebDAV upload intent URL is not canonical".to_string());
    }
    if attempt.username.len() > 512 || attempt.device_id.is_empty() || attempt.device_id.len() > 128
    {
        return Err("WebDAV upload intent identity exceeds its limits".to_string());
    }
    validate_remote_path(&attempt.remote_path)?;
    if let Some(etag) = attempt.expected_etag.as_deref() {
        validate_etag(etag)?;
    }
    Ok(attempt)
}

fn load_upload_attempt(app_data: &Path) -> Result<Option<WebDavUploadAttempt>, String> {
    let path = attempt_journal_path(app_data);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(metadata) if metadata.len() > MAX_ATTEMPT_BYTES => {
            Err("WebDAV upload intent exceeds its size limit".to_string())
        }
        Ok(_) => {
            let bytes = fs::read(&path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
            let attempt = serde_json::from_slice(&bytes)
                .map_err(|error| format!("WebDAV upload intent is invalid: {error}"))?;
            validate_upload_attempt(attempt).map(Some)
        }
    }
}

fn save_upload_attempt(app_data: &Path, attempt: &WebDavUploadAttempt) -> Result<(), String> {
    let attempt = validate_upload_attempt(attempt.clone())?;
    ensure_private_backup_directory(app_data)?;
    let bytes = serde_json::to_vec(&attempt).map_err(command_error)?;
    atomic_write_private(&attempt_journal_path(app_data), &bytes)
}

fn remove_regular_if_present(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(format!("{} is not a regular file", path.display()))
        }
        Ok(_) => fs::remove_file(path)
            .map_err(|error| format!("Failed to remove {}: {error}", path.display())),
    }
}

fn clear_upload_attempt(app_data: &Path) -> Result<(), String> {
    let root = ensure_private_backup_directory(app_data)?;
    remove_regular_if_present(&attempt_journal_path(app_data))?;
    remove_regular_if_present(&attempt_snapshot_path(app_data))?;
    sync_directory(&root)
}

fn create_upload_attempt(
    app_data: &Path,
    config: &WebDavBackupConfig,
    snapshot_sha256: &str,
    bytes: &[u8],
    now: u64,
) -> Result<WebDavUploadAttempt, String> {
    let attempt = WebDavUploadAttempt {
        schema_version: 1,
        attempt_id: Uuid::new_v4().to_string(),
        created_at_ms: now,
        snapshot_sha256: snapshot_sha256.to_string(),
        base_url: config.base_url.clone(),
        username: config.username.clone(),
        remote_path: config.remote_path.clone(),
        device_id: config.device_id.clone(),
        expected_etag: config.known_etag.clone(),
        dispatch_started: false,
    };
    ensure_private_backup_directory(app_data)?;
    // Publish immutable attempt bytes before its journal. A crash between
    // these writes leaves an unreachable file, never an intent without bytes.
    atomic_write_private(&attempt_snapshot_path(app_data), bytes)?;
    save_upload_attempt(app_data, &attempt)?;
    Ok(attempt)
}

fn attempt_target_config(attempt: &WebDavUploadAttempt) -> WebDavBackupConfig {
    WebDavBackupConfig {
        enabled: true,
        base_url: attempt.base_url.clone(),
        username: attempt.username.clone(),
        remote_path: attempt.remote_path.clone(),
        device_id: attempt.device_id.clone(),
        interval_minutes: 5,
        known_etag: attempt.expected_etag.clone(),
        last_attempt_ms: None,
        last_success_ms: None,
        next_due_ms: None,
        last_uploaded_sha256: None,
        last_uploaded_remote_path: None,
        last_error: None,
        consecutive_failures: 0,
    }
}

fn target_matches_attempt(config: &WebDavBackupConfig, attempt: &WebDavUploadAttempt) -> bool {
    config.base_url == attempt.base_url
        && config.username == attempt.username
        && config.remote_path == attempt.remote_path
        && config.device_id == attempt.device_id
}

enum RemoteSnapshotProbe {
    Missing,
    Found { etag: String, bytes: Vec<u8> },
}

async fn probe_remote_snapshot(
    client: &reqwest::Client,
    config: &WebDavBackupConfig,
    password: &str,
    remote_path: &str,
) -> Result<RemoteSnapshotProbe, String> {
    let response = crate::egress::send(
        client.get(remote_url(config, remote_path)?).basic_auth(&config.username, Some(password)),
    )
    .await
    .map_err(command_error)?;
    if response.status().is_redirection() {
        return Err("WebDAV redirects are disabled to protect credentials".to_string());
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(RemoteSnapshotProbe::Missing);
    }
    if !response.status().is_success() {
        return Err(format!(
            "WebDAV reconciliation returned HTTP {}",
            response.status()
        ));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .ok_or_else(|| "WebDAV reconciliation response has no ETag".to_string())?
        .to_str()
        .map_err(command_error)?
        .to_string();
    validate_etag(&etag)?;
    let max = ImportLimits::default().max_archive_bytes.saturating_mul(2);
    if response.content_length().is_some_and(|length| length > max) {
        return Err("WebDAV reconciliation snapshot exceeds the size limit".to_string());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(command_error)?;
        let next = u64::try_from(bytes.len())
            .map_err(command_error)?
            .checked_add(u64::try_from(chunk.len()).map_err(command_error)?)
            .ok_or_else(|| "WebDAV reconciliation size overflow".to_string())?;
        if next > max {
            return Err("WebDAV reconciliation snapshot exceeds the size limit".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(RemoteSnapshotProbe::Found { etag, bytes })
}

fn record_backup_failure(app_data: &Path, now: u64, error: &str) -> Result<(), String> {
    let mut config = load_webdav_config_at(app_data)?;
    config.last_attempt_ms = Some(now);
    config.last_error = Some(error.chars().take(4_096).collect());
    config.consecutive_failures = config.consecutive_failures.saturating_add(1);
    config.next_due_ms = config
        .enabled
        .then_some(now.saturating_add(BACKUP_RETRY_DELAY_MS));
    save_config_file_at(app_data, &config)
}

fn advance_unchanged_snapshot(
    app_data: &Path,
    mut config: WebDavBackupConfig,
    snapshot_sha256: String,
    verified_remote_path: String,
    verified_etag: String,
    now: u64,
) -> Result<WebDavBackgroundRunOutcome, String> {
    if verified_remote_path == config.remote_path {
        config.known_etag = Some(verified_etag);
    }
    config.last_uploaded_remote_path = Some(verified_remote_path);
    config.last_error = None;
    config.consecutive_failures = 0;
    let next_due_ms = now.saturating_add(config.interval_minutes.saturating_mul(60_000));
    config.next_due_ms = Some(next_due_ms);
    save_config_file_at(app_data, &config)?;
    Ok(WebDavBackgroundRunOutcome::AlreadyCurrent {
        snapshot_sha256,
        next_due_ms,
    })
}

fn finish_background_upload(
    app_data: &Path,
    attempt: &WebDavUploadAttempt,
    remote_path: String,
    etag: String,
    conflicting_etag: Option<String>,
    conflict_copy: bool,
    now: u64,
) -> Result<WebDavBackgroundRunOutcome, String> {
    let mut config = load_webdav_config_at(app_data)?;
    if target_matches_attempt(&config, attempt) && !conflict_copy {
        config.known_etag = Some(etag.clone());
    }
    config.last_attempt_ms = Some(now);
    config.last_success_ms = Some(now);
    config.last_uploaded_sha256 = Some(attempt.snapshot_sha256.clone());
    config.last_uploaded_remote_path = Some(remote_path.clone());
    config.last_error = None;
    config.consecutive_failures = 0;
    config.next_due_ms = config
        .enabled
        .then_some(now.saturating_add(config.interval_minutes.saturating_mul(60_000)));
    // Commit success before clearing the intent. If the process dies between
    // these operations, last_uploaded_sha256 proves that the old intent can be
    // removed without another network mutation.
    save_config_file_at(app_data, &config)?;
    clear_upload_attempt(app_data)?;
    if conflict_copy {
        Ok(WebDavBackgroundRunOutcome::ConflictCopy {
            remote_path,
            etag,
            conflicting_etag,
            snapshot_sha256: attempt.snapshot_sha256.clone(),
        })
    } else {
        Ok(WebDavBackgroundRunOutcome::Uploaded {
            remote_path,
            etag,
            snapshot_sha256: attempt.snapshot_sha256.clone(),
        })
    }
}

async fn run_claimed_background_upload(
    app_data: &Path,
    now: u64,
) -> Result<WebDavBackgroundRunOutcome, String> {
    let mut config = load_webdav_config_at(app_data)?;
    if !config.enabled {
        return Ok(WebDavBackgroundRunOutcome::Disabled);
    }

    let mut pending = load_upload_attempt(app_data)?;
    if pending.as_ref().is_some_and(|attempt| {
        config.last_uploaded_sha256.as_deref() == Some(attempt.snapshot_sha256.as_str())
    }) {
        clear_upload_attempt(app_data)?;
        pending = None;
    }

    let (attempt, bytes) = if let Some(attempt) = pending {
        let bytes = read_regular_bounded(
            &attempt_snapshot_path(app_data),
            ImportLimits::default().max_archive_bytes.saturating_mul(2),
        )?;
        if sha256_hex(&bytes) != attempt.snapshot_sha256 {
            return Err("Durable WebDAV upload intent bytes failed their digest check".to_string());
        }
        (attempt, bytes)
    } else {
        let Some((staged, bytes)) = read_staged_snapshot(app_data)? else {
            record_backup_failure(app_data, now, "No encrypted frontend snapshot is staged")?;
            return Ok(WebDavBackgroundRunOutcome::MissingStagedSource);
        };
        if config.last_uploaded_sha256.as_deref() == Some(staged.sha256.as_str()) {
            let crypto = KeychainAes256Gcm::load_or_create()?;
            open_encrypted_snapshot(&bytes, &ImportLimits::default(), &crypto)
                .map_err(command_error)?;
            let current_target = validate_stored_config(config.clone())?;
            let password = load_webdav_password(&current_target)?;
            let client = webdav_client()?;
            let verified_path = config
                .last_uploaded_remote_path
                .clone()
                .unwrap_or_else(|| config.remote_path.clone());
            match probe_remote_snapshot(&client, &current_target, &password, &verified_path).await?
            {
                RemoteSnapshotProbe::Found {
                    etag,
                    bytes: remote,
                } if sha256_hex(&remote) == staged.sha256 => {
                    return advance_unchanged_snapshot(
                        app_data,
                        config,
                        staged.sha256,
                        verified_path,
                        etag,
                        now,
                    );
                }
                RemoteSnapshotProbe::Missing if verified_path == config.remote_path => {
                    // The primary was deleted out of band. Re-create it with
                    // If-None-Match instead of turning a stale ETag into a
                    // misleading conflict copy.
                    config.known_etag = None;
                }
                _ => {}
            }
        }
        let attempt = create_upload_attempt(app_data, &config, &staged.sha256, &bytes, now)?;
        (attempt, bytes)
    };

    let crypto = KeychainAes256Gcm::load_or_create()?;
    open_encrypted_snapshot(&bytes, &ImportLimits::default(), &crypto).map_err(command_error)?;
    let target = validate_stored_config(attempt_target_config(&attempt))?;
    let password = load_webdav_password(&target)?;
    let client = webdav_client()?;

    if attempt.dispatch_started {
        if let RemoteSnapshotProbe::Found {
            etag,
            bytes: remote,
        } = probe_remote_snapshot(&client, &target, &password, &attempt.remote_path).await?
        {
            if sha256_hex(&remote) == attempt.snapshot_sha256 {
                return finish_background_upload(
                    app_data,
                    &attempt,
                    attempt.remote_path.clone(),
                    etag,
                    None,
                    false,
                    now,
                );
            }
        }
    }

    config = load_webdav_config_at(app_data)?;
    config.last_attempt_ms = Some(now);
    config.last_error = None;
    save_config_file_at(app_data, &config)?;
    let mut dispatched = attempt.clone();
    dispatched.dispatch_started = true;
    // The durable marker precedes the HTTP mutation. Recovery probes the
    // exact remote bytes before deciding whether it is safe to retry.
    save_upload_attempt(app_data, &dispatched)?;

    let (status, etag) = put_webdav(
        &client,
        &target,
        &password,
        &attempt.remote_path,
        &bytes,
        attempt.expected_etag.as_deref(),
    )
    .await?;
    if status.is_success() {
        let etag = etag.ok_or_else(|| {
            "WebDAV upload succeeded without an ETag; crash-safe reconciliation is unavailable"
                .to_string()
        })?;
        return finish_background_upload(
            app_data,
            &dispatched,
            dispatched.remote_path.clone(),
            etag,
            None,
            false,
            now,
        );
    }
    if status != reqwest::StatusCode::PRECONDITION_FAILED {
        return Err(format!("WebDAV upload returned HTTP {status}"));
    }

    let latest =
        probe_remote_snapshot(&client, &target, &password, &dispatched.remote_path).await?;
    let conflicting_etag = match latest {
        RemoteSnapshotProbe::Found {
            etag,
            bytes: remote,
        } if sha256_hex(&remote) == dispatched.snapshot_sha256 => {
            return finish_background_upload(
                app_data,
                &dispatched,
                dispatched.remote_path.clone(),
                etag,
                None,
                false,
                now,
            );
        }
        RemoteSnapshotProbe::Found { etag, .. } => Some(etag),
        RemoteSnapshotProbe::Missing => None,
    };
    let conflict = conflict_remote_path(
        &dispatched.remote_path,
        &dispatched.device_id,
        dispatched.created_at_ms,
        &bytes,
    )?;
    if let RemoteSnapshotProbe::Found {
        etag,
        bytes: remote,
    } = probe_remote_snapshot(&client, &target, &password, &conflict).await?
    {
        if sha256_hex(&remote) == dispatched.snapshot_sha256 {
            return finish_background_upload(
                app_data,
                &dispatched,
                conflict,
                etag,
                conflicting_etag,
                true,
                now,
            );
        }
        return Err("Deterministic WebDAV conflict path exists with different bytes".to_string());
    }

    let (copy_status, copy_etag) =
        put_webdav(&client, &target, &password, &conflict, &bytes, None).await?;
    if copy_status == reqwest::StatusCode::PRECONDITION_FAILED {
        if let RemoteSnapshotProbe::Found {
            etag,
            bytes: remote,
        } = probe_remote_snapshot(&client, &target, &password, &conflict).await?
        {
            if sha256_hex(&remote) == dispatched.snapshot_sha256 {
                return finish_background_upload(
                    app_data,
                    &dispatched,
                    conflict,
                    etag,
                    conflicting_etag,
                    true,
                    now,
                );
            }
        }
        return Err("WebDAV conflict-copy path was concurrently claimed".to_string());
    }
    if !copy_status.is_success() {
        return Err(format!(
            "WebDAV conflict-copy upload returned HTTP {copy_status}"
        ));
    }
    let copy_etag =
        copy_etag.ok_or_else(|| "WebDAV conflict copy succeeded without an ETag".to_string())?;
    finish_background_upload(
        app_data,
        &dispatched,
        conflict,
        copy_etag,
        conflicting_etag,
        true,
        now,
    )
}

/// Runs one durable WebDAV schedule check for either the resident daemon or
/// the desktop catch-up loop. SQLite owns the cross-process lease; encrypted
/// intent bytes and a pre-dispatch journal own crash reconciliation.
pub async fn run_due_webdav_backup(
    app_data: &Path,
    owner: &str,
    now: u64,
    force: bool,
) -> Result<WebDavBackgroundRunOutcome, String> {
    let config = load_webdav_config_at(app_data)?;
    if !config.enabled {
        return Ok(WebDavBackgroundRunOutcome::Disabled);
    }
    if !force {
        if let Some(next_due_ms) = config.next_due_ms {
            if next_due_ms > now {
                return Ok(WebDavBackgroundRunOutcome::NotDue { next_due_ms });
            }
        }
    }
    let mut claim = match acquire_backup_claim(app_data, owner, now)? {
        BackupClaimOutcome::Busy(info) => {
            return Ok(WebDavBackgroundRunOutcome::Busy {
                owner: info.owner,
                expires_at_ms: info.expires_at_ms,
            })
        }
        BackupClaimOutcome::Acquired(claim) => claim,
    };
    let result = run_claimed_background_upload(app_data, now).await;
    if let Err(error) = &result {
        if let Err(persist_error) = record_backup_failure(app_data, now, error) {
            claim.release()?;
            return Err(format!(
                "{error}; additionally failed to persist backup error state: {persist_error}"
            ));
        }
    }
    claim.release()?;
    result
}

#[tauri::command]
pub async fn portable_webdav_run_due(
    app: tauri::AppHandle,
    force: bool,
) -> Result<WebDavBackgroundRunOutcome, String> {
    let app_data = app.path().app_data_dir().map_err(command_error)?;
    run_due_webdav_backup(
        &app_data,
        &format!("desktop-{}-{}", std::process::id(), Uuid::new_v4()),
        now_ms()?,
        force,
    )
    .await
}

#[tauri::command]
pub async fn portable_webdav_test(app: tauri::AppHandle) -> Result<(), String> {
    let config = validate_stored_config(load_webdav_config(&app)?)?;
    let password = load_webdav_password(&config)?;
    let response = crate::egress::send(
        webdav_client()?
            .request(
                reqwest::Method::OPTIONS,
                Url::parse(&config.base_url).map_err(command_error)?,
            )
            .basic_auth(&config.username, Some(password)),
    )
    .await
    .map_err(command_error)?;
    if response.status().is_redirection() {
        return Err("WebDAV redirects are disabled to protect credentials".to_string());
    }
    if !response.status().is_success() {
        return Err(format!("WebDAV server returned HTTP {}", response.status()));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebDavDownloadResponse {
    Downloaded {
        remote_path: String,
        etag: String,
        payload: PortableReadOutcome,
    },
    NotModified,
    Missing,
}

#[tauri::command]
pub async fn portable_webdav_download_snapshot(
    app: tauri::AppHandle,
) -> Result<WebDavDownloadResponse, String> {
    let app_data = app.path().app_data_dir().map_err(command_error)?;
    let mut claim = match acquire_backup_claim(
        &app_data,
        &format!("desktop-download-{}", Uuid::new_v4()),
        now_ms()?,
    )? {
        BackupClaimOutcome::Busy(info) => {
            return Err(format!(
                "WebDAV backup is already active in {} until {}",
                info.owner, info.expires_at_ms
            ))
        }
        BackupClaimOutcome::Acquired(claim) => claim,
    };
    let mut config = validate_stored_config(load_webdav_config(&app)?)?;
    let password = load_webdav_password(&config)?;
    let mut request = webdav_client()?
        .get(remote_url(&config, &config.remote_path)?)
        .basic_auth(&config.username, Some(password));
    if let Some(etag) = config.known_etag.as_deref() {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = crate::egress::send(request).await.map_err(command_error)?;
    if response.status().is_redirection() {
        return Err("WebDAV redirects are disabled to protect credentials".to_string());
    }
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        claim.release()?;
        return Ok(WebDavDownloadResponse::NotModified);
    }
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        claim.release()?;
        return Ok(WebDavDownloadResponse::Missing);
    }
    if !response.status().is_success() {
        return Err(format!(
            "WebDAV download returned HTTP {}",
            response.status()
        ));
    }
    let etag = response
        .headers()
        .get(ETAG)
        .ok_or_else(|| "WebDAV download response has no ETag".to_string())?
        .to_str()
        .map_err(command_error)?
        .to_string();
    validate_etag(&etag)?;
    let max = ImportLimits::default().max_archive_bytes.saturating_mul(2);
    if response.content_length().is_some_and(|length| length > max) {
        return Err("WebDAV snapshot exceeds the configured download limit".to_string());
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(command_error)?;
        let next = u64::try_from(bytes.len())
            .map_err(command_error)?
            .checked_add(u64::try_from(chunk.len()).map_err(command_error)?)
            .ok_or_else(|| "WebDAV snapshot size overflow".to_string())?;
        if next > max {
            return Err("WebDAV snapshot exceeds the configured download limit".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    let crypto = KeychainAes256Gcm::load_or_create()?;
    let opened = open_encrypted_snapshot(&bytes, &ImportLimits::default(), &crypto)
        .map_err(command_error)?;
    let media_types = opened
        .bundle
        .manifest
        .artifacts
        .iter()
        .map(|descriptor| (descriptor.id.clone(), descriptor.media_type.clone()))
        .collect::<std::collections::HashMap<_, _>>();
    let artifacts = opened
        .bundle
        .artifacts
        .into_iter()
        .map(|(id, bytes)| PortableArtifactResponse {
            media_type: media_types
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "application/octet-stream".to_string()),
            id,
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
        .collect();
    let payload = PortableReadOutcome {
        data: opened.bundle.data,
        artifacts,
        preflight: opened.preflight,
    };
    config.known_etag = Some(etag.clone());
    let timestamp = now_ms()?;
    config.last_attempt_ms = Some(timestamp);
    config.last_success_ms = Some(timestamp);
    config.next_due_ms =
        Some(timestamp.saturating_add(config.interval_minutes.saturating_mul(60_000)));
    config.last_error = None;
    config.consecutive_failures = 0;
    save_config_file(&app, &config)?;
    claim.release()?;
    Ok(WebDavDownloadResponse::Downloaded {
        remote_path: config.remote_path,
        etag,
        payload,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RestoreTestDirectory {
        path: PathBuf,
    }

    impl RestoreTestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-portable-restore-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(path.join("stacks")).unwrap();
            Self { path }
        }
    }

    impl Drop for RestoreTestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn profile_payload(session_id: &str, title: &str) -> String {
        serde_json::json!({
            "sessions": [{
                "id": session_id,
                "title": title,
                "messages": [],
                "createdAt": 1,
                "updatedAt": 1,
                "pinned": false,
                "unread": false,
                "archived": false,
                "groupId": null,
                "modelTarget": null,
                "workspacePath": null,
                "personaId": null,
                "subagentRuns": {}
            }],
            "activeSessionId": session_id,
            "groups": [],
            "crews": []
        })
        .to_string()
    }

    fn prompt_payload(id: &str, content: &str) -> String {
        serde_json::json!({
            "version": 1,
            "entries": [{
                "id": id,
                "kind": "snippet",
                "name": "Fixture",
                "command": id,
                "content": content,
                "createdAt": 1,
                "updatedAt": 1
            }],
            "defaultPersonaId": null,
            "hasSeededDefaults": true
        })
        .to_string()
    }

    fn stack(id: &str, name: &str) -> crate::knowledge_core::KnowledgeStack {
        crate::knowledge_core::KnowledgeStack {
            id: id.to_string(),
            name: name.to_string(),
            sources: Vec::new(),
            embedding: crate::knowledge_core::EmbeddingSpec {
                backend: crate::knowledge_core::EmbeddingBackend::Ollama,
                model_id_or_tag: "nomic-embed-text".to_string(),
                dim: 768,
                query_prefix: "search_query: ".to_string(),
                doc_prefix: "search_document: ".to_string(),
            },
            chunk_chars: 1_600,
            chunk_overlap: 200,
            indexed_at: None,
            chunk_count: 0,
        }
    }

    fn seed_restore_files(base: &Path) -> (String, String, Vec<u8>, Vec<u8>) {
        let sessions = profile_payload("old-session", "Old");
        let prompts = prompt_payload("old-prompt", "old");
        let stacks = serde_json::to_vec(&vec![stack("old-stack", "Old stack")]).unwrap();
        let settings = serde_json::to_vec(&PendingPortableRestoreSettings {
            schema_version: 1,
            transaction_id: Uuid::new_v4().to_string(),
            locale: Some("en".to_string()),
            shortcut_overrides: Some(serde_json::json!({})),
        })
        .unwrap();
        fs::write(restore_target(base, RestoreFileKind::Sessions), &sessions).unwrap();
        fs::write(restore_target(base, RestoreFileKind::Prompts), &prompts).unwrap();
        fs::write(restore_target(base, RestoreFileKind::Stacks), &stacks).unwrap();
        fs::write(
            restore_target(base, RestoreFileKind::PendingSettings),
            &settings,
        )
        .unwrap();
        (sessions, prompts, stacks, settings)
    }

    fn restore_request(previous_sessions_payload: String) -> PortableRestoreRequest {
        PortableRestoreRequest {
            mode: PortableRestoreMode::Replace,
            sessions_payload: profile_payload("new-session", "New"),
            previous_sessions_payload,
            prompts_payload: prompt_payload("new-prompt", "new"),
            stacks: vec![stack("new-stack", "New stack")],
            settings: Some(PortableRestoreSettingsRequest {
                locale: Some("sv-SE".to_string()),
                shortcut_overrides: Some(serde_json::json!({
                    "commandPalette.open": [{"key": "k", "meta": true}]
                })),
            }),
        }
    }

    #[test]
    fn aes_gcm_binds_aad_and_rejects_tampering() {
        let mut key = vec![0_u8; 32];
        SystemRandom::new().fill(&mut key).unwrap();
        let provider = KeychainAes256Gcm {
            key: LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).unwrap()),
            key_id: "backup-test-key".to_string(),
        };
        let sealed = provider.seal(b"header", b"portable bundle").unwrap();
        assert_eq!(
            provider
                .open(b"header", &sealed.nonce, &sealed.ciphertext, &sealed.tag)
                .unwrap(),
            b"portable bundle"
        );
        assert!(provider
            .open(b"other", &sealed.nonce, &sealed.ciphertext, &sealed.tag)
            .is_err());
        let mut tampered = sealed.ciphertext;
        tampered[0] ^= 1;
        assert!(provider
            .open(b"header", &sealed.nonce, &tampered, &sealed.tag)
            .is_err());
    }

    #[test]
    fn atomic_replacement_updates_an_existing_private_state_file() {
        let directory = RestoreTestDirectory::new("atomic-replace");
        let path = directory.path.join("state.json");
        fs::write(&path, b"old").unwrap();
        atomic_write_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert!(fs::read_dir(&directory.path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp")));
    }

    #[test]
    fn webdav_policy_requires_tls_and_safe_relative_paths() {
        assert!(normalized_webdav_base("https://dav.example.test/root").is_ok());
        assert!(normalized_webdav_base("http://127.0.0.1:8080/root").is_ok());
        assert!(normalized_webdav_base("http://dav.example.test/root").is_err());
        assert!(normalized_webdav_base("https://user:secret@dav.example.test").is_err());
        assert!(validate_remote_path("LittleMonkey/latest.lmsnapshot").is_ok());
        assert!(validate_remote_path("../secret").is_err());
        assert!(validate_remote_path("folder\\file").is_err());
    }

    #[test]
    fn conflict_path_is_deterministic_and_keeps_extension() {
        let first =
            conflict_remote_path("backup/latest.lmsnapshot", "device-a", 42, b"same").unwrap();
        let second =
            conflict_remote_path("backup/latest.lmsnapshot", "device-a", 42, b"same").unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("backup/latest.conflict-"));
        assert!(first.ends_with(".lmsnapshot"));
    }

    #[test]
    fn webdav_claim_is_cross_process_exclusive_and_old_lease_cannot_delete_successor() {
        let directory = RestoreTestDirectory::new("webdav-claim");
        let now = 10_000;
        let mut first = match acquire_backup_claim(&directory.path, "desktop-a", now).unwrap() {
            BackupClaimOutcome::Acquired(claim) => claim,
            BackupClaimOutcome::Busy(_) => panic!("first owner must acquire the claim"),
        };
        match acquire_backup_claim(&directory.path, "daemon-b", now + 1).unwrap() {
            BackupClaimOutcome::Busy(info) => {
                assert_eq!(info.owner, "desktop-a");
                assert_eq!(info.expires_at_ms, now + BACKUP_CLAIM_TTL_MS);
            }
            BackupClaimOutcome::Acquired(_) => panic!("live claim must be exclusive"),
        }
        let mut successor =
            match acquire_backup_claim(&directory.path, "daemon-b", now + BACKUP_CLAIM_TTL_MS + 1)
                .unwrap()
            {
                BackupClaimOutcome::Acquired(claim) => claim,
                BackupClaimOutcome::Busy(_) => panic!("expired lease must be recoverable"),
            };
        first.release().unwrap();
        let current = current_backup_claim(&directory.path, now + BACKUP_CLAIM_TTL_MS + 2)
            .unwrap()
            .unwrap();
        assert_eq!(current.owner, "daemon-b");
        successor.release().unwrap();
        assert!(
            current_backup_claim(&directory.path, now + BACKUP_CLAIM_TTL_MS + 2)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn upload_intent_publishes_immutable_bytes_and_survives_restart() {
        let directory = RestoreTestDirectory::new("webdav-intent");
        let mut config = WebDavBackupConfig::default();
        config.enabled = true;
        config.base_url = "https://dav.example.test/root/".to_string();
        config.username = "user".to_string();
        let bytes = br#"{"encrypted":"fixture"}"#;
        let digest = sha256_hex(bytes);
        let attempt = create_upload_attempt(&directory.path, &config, &digest, bytes, 42).unwrap();
        let loaded = load_upload_attempt(&directory.path).unwrap().unwrap();
        assert_eq!(loaded.attempt_id, attempt.attempt_id);
        assert_eq!(loaded.snapshot_sha256, digest);
        assert_eq!(
            fs::read(attempt_snapshot_path(&directory.path)).unwrap(),
            bytes
        );

        let mut dispatched = loaded;
        dispatched.dispatch_started = true;
        save_upload_attempt(&directory.path, &dispatched).unwrap();
        assert!(
            load_upload_attempt(&directory.path)
                .unwrap()
                .unwrap()
                .dispatch_started
        );
        clear_upload_attempt(&directory.path).unwrap();
        assert!(load_upload_attempt(&directory.path).unwrap().is_none());
        assert!(!attempt_snapshot_path(&directory.path).exists());
    }

    #[test]
    fn old_config_deserializes_new_daemon_state_with_safe_defaults() {
        let config: WebDavBackupConfig = serde_json::from_value(serde_json::json!({
            "enabled": false,
            "baseUrl": "",
            "username": "",
            "remotePath": "LittleMonkey/latest.lmsnapshot",
            "deviceId": "device-legacy",
            "intervalMinutes": 1440,
            "knownEtag": null,
            "lastAttemptMs": null,
            "lastSuccessMs": null,
            "nextDueMs": null
        }))
        .unwrap();
        assert_eq!(config.last_uploaded_sha256, None);
        assert_eq!(config.last_uploaded_remote_path, None);
        assert_eq!(config.last_error, None);
        assert_eq!(config.consecutive_failures, 0);
    }

    #[test]
    fn background_failure_is_bounded_and_schedules_a_retry() {
        let directory = RestoreTestDirectory::new("webdav-retry");
        let mut config = WebDavBackupConfig::default();
        config.enabled = true;
        config.base_url = "https://dav.example.test/root/".to_string();
        save_config_file_at(&directory.path, &config).unwrap();
        record_backup_failure(&directory.path, 50_000, &"x".repeat(8_000)).unwrap();
        let stored = load_webdav_config_at(&directory.path).unwrap();
        assert_eq!(stored.last_attempt_ms, Some(50_000));
        assert_eq!(stored.next_due_ms, Some(50_000 + BACKUP_RETRY_DELAY_MS));
        assert_eq!(stored.consecutive_failures, 1);
        assert_eq!(stored.last_error.unwrap().chars().count(), 4_096);
    }

    #[tokio::test]
    async fn daemon_entrypoint_is_opt_in_and_missing_stage_is_visible_without_credentials() {
        let disabled = RestoreTestDirectory::new("webdav-disabled");
        assert!(matches!(
            run_due_webdav_backup(&disabled.path, "daemon-test", 10_000, true)
                .await
                .unwrap(),
            WebDavBackgroundRunOutcome::Disabled
        ));

        let enabled = RestoreTestDirectory::new("webdav-missing-stage");
        let mut config = WebDavBackupConfig::default();
        config.enabled = true;
        config.base_url = "https://dav.example.test/root/".to_string();
        config.next_due_ms = Some(1);
        save_config_file_at(&enabled.path, &config).unwrap();
        assert!(matches!(
            run_due_webdav_backup(&enabled.path, "daemon-test", 10_000, false)
                .await
                .unwrap(),
            WebDavBackgroundRunOutcome::MissingStagedSource
        ));
        let stored = load_webdav_config_at(&enabled.path).unwrap();
        assert_eq!(stored.consecutive_failures, 1);
        assert!(stored
            .last_error
            .as_deref()
            .is_some_and(|message| message.contains("No encrypted frontend snapshot")));
        assert!(current_backup_claim(&enabled.path, 10_001)
            .unwrap()
            .is_none());
    }

    #[test]
    fn atomic_restore_publishes_the_complete_profile_write_set() {
        let directory = RestoreTestDirectory::new("commit");
        let (old_sessions, _, _, _) = seed_restore_files(&directory.path);
        let request = restore_request(old_sessions);
        let expected_sessions = request.sessions_payload.clone();
        let expected_prompts = request.prompts_payload.clone();

        let (published, counts, settings_pending) =
            publish_restore_at(&directory.path, request, None).unwrap();
        assert_eq!(counts.sessions, 1);
        assert!(settings_pending);
        assert_eq!(
            fs::read_to_string(restore_target(&directory.path, RestoreFileKind::Sessions)).unwrap(),
            expected_sessions
        );
        assert_eq!(
            fs::read_to_string(restore_target(&directory.path, RestoreFileKind::Prompts)).unwrap(),
            expected_prompts
        );
        let stacks: Vec<crate::knowledge_core::KnowledgeStack> = serde_json::from_slice(
            &fs::read(restore_target(&directory.path, RestoreFileKind::Stacks)).unwrap(),
        )
        .unwrap();
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].id, "new-stack");
        let pending: PendingPortableRestoreSettings = serde_json::from_slice(
            &fs::read(restore_target(
                &directory.path,
                RestoreFileKind::PendingSettings,
            ))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(pending.locale.as_deref(), Some("sv-SE"));

        published.mark_committed().unwrap();
        published.cleanup();
        assert!(!published.transaction_root.exists());
    }

    #[test]
    fn injected_publish_failure_rolls_back_every_profile_file() {
        let directory = RestoreTestDirectory::new("rollback");
        let (sessions, prompts, stacks, settings) = seed_restore_files(&directory.path);
        let error = publish_restore_at(&directory.path, restore_request(sessions.clone()), Some(2))
            .expect_err("the injected failure must abort the transaction");
        assert!(error.contains("Injected portable restore failure"));
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Sessions)).unwrap(),
            sessions.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Prompts)).unwrap(),
            prompts.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Stacks)).unwrap(),
            stacks
        );
        assert_eq!(
            fs::read(restore_target(
                &directory.path,
                RestoreFileKind::PendingSettings
            ))
            .unwrap(),
            settings
        );
        assert!(fs::read_dir(&directory.path).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(RESTORE_TRANSACTION_PREFIX)));
    }

    #[test]
    fn startup_recovery_rolls_back_an_uncommitted_complete_publish() {
        let directory = RestoreTestDirectory::new("recovery");
        let (sessions, prompts, stacks, settings) = seed_restore_files(&directory.path);
        let (published, _, _) =
            publish_restore_at(&directory.path, restore_request(sessions.clone()), None).unwrap();
        assert_eq!(
            fs::read_to_string(restore_target(&directory.path, RestoreFileKind::Sessions)).unwrap(),
            profile_payload("new-session", "New")
        );
        assert!(published.transaction_root.exists());

        let rollback_profiles = recover_restore_transactions_at(&directory.path).unwrap();
        assert_eq!(rollback_profiles, vec![sessions.clone()]);
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Sessions)).unwrap(),
            sessions.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Prompts)).unwrap(),
            prompts.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Stacks)).unwrap(),
            stacks
        );
        assert_eq!(
            fs::read(restore_target(
                &directory.path,
                RestoreFileKind::PendingSettings
            ))
            .unwrap(),
            settings
        );
    }

    #[test]
    fn restore_prevalidation_rejects_malformed_payloads_before_mutation() {
        let directory = RestoreTestDirectory::new("preflight");
        let (sessions, prompts, stacks, settings) = seed_restore_files(&directory.path);
        let mut request = restore_request(sessions.clone());
        request.sessions_payload = "{not-json".to_string();
        assert!(publish_restore_at(&directory.path, request, None).is_err());
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Sessions)).unwrap(),
            sessions.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Prompts)).unwrap(),
            prompts.as_bytes()
        );
        assert_eq!(
            fs::read(restore_target(&directory.path, RestoreFileKind::Stacks)).unwrap(),
            stacks
        );
        assert_eq!(
            fs::read(restore_target(
                &directory.path,
                RestoreFileKind::PendingSettings
            ))
            .unwrap(),
            settings
        );
    }
}
