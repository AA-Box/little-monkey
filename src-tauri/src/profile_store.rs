//! Defensive migration and indexed search for the legacy chat profile.
//!
//! `chat_sessions.json` remains the frontend wire format while this module
//! normalizes it into the shared SQLite database. Parsing, bounds checks, and
//! data-URL extraction all finish before a database transaction starts. The
//! APIs are synchronous and Tauri-free so desktop, CLI, daemon, and tests use
//! the same migration and search contract.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use rusqlite::{
    params, params_from_iter, types::Value as SqlValue, OptionalExtension, Transaction,
    TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::artifact_store::{ArtifactStore, ArtifactStoreError};
use crate::run_ledger::{LedgerError, RunLedger};
use crate::run_protocol::{RunEvent, RunEventEnvelope, RunSpec};

pub const MAX_PROFILE_JSON_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_PROFILE_SESSIONS: usize = 10_000;
pub const MAX_PROFILE_GROUPS: usize = 10_000;
pub const MAX_PROFILE_CREWS: usize = 10_000;
pub const MAX_PROFILE_MESSAGES: usize = 1_000_000;
pub const MAX_MESSAGE_TEXT_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ENTITY_METADATA_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_TOTAL_METADATA_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_SEARCH_LIMIT: usize = 100;
pub const MAX_SEARCH_QUERY_BYTES: usize = 512;
pub const MAX_SEARCH_SNIPPET_CHARS: usize = 1_024;
pub const MAX_SEARCH_ARTIFACT_BYTES: u64 = 2 * 1024 * 1024;

const MAX_ID_BYTES: usize = 256;
const MAX_TITLE_BYTES: usize = 16 * 1024;
const MAX_PATH_BYTES: usize = 16 * 1024;
const MAX_CONTENT_PARTS: usize = 256;
const MAX_JSON_NODES: usize = 2_000_000;
const ORDINAL_SHIFT: i64 = 1_000_000_000;

pub type ProfileStoreResult<T> = Result<T, ProfileStoreError>;

#[derive(Debug)]
pub enum ProfileStoreError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    Sqlite(rusqlite::Error),
    Ledger(LedgerError),
    Artifact(ArtifactStoreError),
    Invalid {
        path: String,
        message: String,
    },
    InputTooLarge {
        observed: u64,
        max: u64,
    },
    SearchUnavailable,
    Corrupt(String),
}

impl fmt::Display for ProfileStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Json(error) => write!(f, "invalid profile JSON: {error}"),
            Self::Sqlite(error) => write!(f, "profile SQLite error: {error}"),
            Self::Ledger(error) => write!(f, "run ledger error: {error}"),
            Self::Artifact(error) => write!(f, "artifact store error: {error}"),
            Self::Invalid { path, message } => {
                write!(f, "invalid profile value at {path}: {message}")
            }
            Self::InputTooLarge { observed, max } => {
                write!(
                    f,
                    "profile is {observed} bytes, exceeding the {max} byte limit"
                )
            }
            Self::SearchUnavailable => f.write_str("SQLite FTS5 is unavailable"),
            Self::Corrupt(message) => write!(f, "profile database is corrupt: {message}"),
        }
    }
}

impl Error for ProfileStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Ledger(error) => Some(error),
            Self::Artifact(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for ProfileStoreError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<rusqlite::Error> for ProfileStoreError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<LedgerError> for ProfileStoreError {
    fn from(value: LedgerError) -> Self {
        Self::Ledger(value)
    }
}

impl From<ArtifactStoreError> for ProfileStoreError {
    fn from(value: ArtifactStoreError) -> Self {
        Self::Artifact(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    SourceMissing,
    Pending,
    Current,
    SourceChanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMigrationStatus {
    pub state: MigrationState,
    pub source_path: PathBuf,
    pub source_sha256: Option<String>,
    pub imported_sha256: Option<String>,
    pub recovery_path: Option<PathBuf>,
    pub migrated_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationOutcome {
    Imported,
    Updated,
    NoChange,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCounts {
    pub groups: usize,
    pub sessions: usize,
    pub messages: usize,
    pub actor_transcripts: usize,
    pub crews: usize,
    pub attachment_occurrences: usize,
    pub unique_artifacts: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMigrationResult {
    pub outcome: MigrationOutcome,
    pub source_sha256: String,
    pub payload_sha256: String,
    pub recovery_path: Option<PathBuf>,
    pub counts: ProfileCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSaveResult {
    pub changed: bool,
    pub payload_sha256: String,
    pub counts: ProfileCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSourceKind {
    Message,
    ActorTranscript,
    RunEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct GlobalSearchRequest {
    pub query: String,
    pub include_archived: bool,
    pub from_ms: Option<u64>,
    pub to_ms: Option<u64>,
    pub model_key: Option<String>,
    pub persona_id: Option<String>,
    pub workspace_path: Option<String>,
    pub limit: usize,
}

impl Default for GlobalSearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            include_archived: false,
            from_ms: None,
            to_ms: None,
            model_key: None,
            persona_id: None,
            workspace_path: None,
            limit: 25,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalSearchHit {
    pub document_id: String,
    pub source_kind: SearchSourceKind,
    pub source_id: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub title: String,
    pub role: String,
    pub snippet: String,
    pub occurred_at_ms: u64,
    pub model_key: Option<String>,
    pub persona_id: Option<String>,
    pub workspace_path: Option<String>,
    pub archived: bool,
    pub score: f64,
}

#[derive(Debug)]
struct NormalizedProfile {
    active_session_id: String,
    root_metadata_json: Option<Vec<u8>>,
    groups: Vec<NormalizedGroup>,
    sessions: Vec<NormalizedSession>,
    crews: Vec<NormalizedCrew>,
    artifacts: BTreeMap<String, PendingArtifact>,
    attachments: BTreeMap<String, NormalizedAttachment>,
    counts: ProfileCounts,
}

#[derive(Debug)]
struct NormalizedGroup {
    id: String,
    ordinal: i64,
    name: String,
    kind: String,
    created_at_ms: i64,
    metadata_json: Option<Vec<u8>>,
}

#[derive(Debug)]
struct NormalizedSession {
    id: String,
    group_id: Option<String>,
    ordinal: i64,
    title: String,
    pinned: bool,
    unread: bool,
    archived: bool,
    created_at_ms: i64,
    updated_at_ms: i64,
    model_key: Option<String>,
    persona_id: Option<String>,
    workspace_path: Option<String>,
    metadata_json: Option<Vec<u8>>,
    messages: Vec<NormalizedMessage>,
    actor_transcripts: Vec<NormalizedTranscript>,
}

#[derive(Debug)]
struct NormalizedMessage {
    id: String,
    ordinal: i64,
    role: String,
    content: String,
    metadata_json: Option<Vec<u8>>,
    created_at_ms: i64,
    updated_at_ms: i64,
    attachments: Vec<MessageAttachmentLink>,
}

#[derive(Debug)]
struct MessageAttachmentLink {
    ordinal: i64,
    attachment_id: String,
    purpose: String,
}

#[derive(Debug)]
struct NormalizedTranscript {
    id: String,
    actor_id: String,
    ordinal: i64,
    kind: String,
    content: String,
    created_at_ms: i64,
    model_key: Option<String>,
    persona_id: Option<String>,
    metadata_json: Option<Vec<u8>>,
}

#[derive(Debug)]
struct NormalizedCrew {
    id: String,
    ordinal: i64,
    name: String,
    metadata_json: Vec<u8>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Debug)]
struct PendingArtifact {
    bytes: Vec<u8>,
}

#[derive(Clone, Debug)]
struct NormalizedAttachment {
    id: String,
    blob_id: String,
    media_type: String,
    byte_size: i64,
}

#[derive(Default)]
struct ParseBudget {
    messages: usize,
    metadata_bytes: usize,
    attachment_bytes: u64,
    json_nodes: usize,
}

struct ParseContext {
    budget: ParseBudget,
    artifacts: BTreeMap<String, PendingArtifact>,
    attachments: BTreeMap<String, NormalizedAttachment>,
}

impl ParseContext {
    fn new() -> Self {
        Self {
            budget: ParseBudget::default(),
            artifacts: BTreeMap::new(),
            attachments: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
struct StoredProfileState {
    source_sha256: Option<String>,
    recovery_path: Option<PathBuf>,
    migrated_at_ms: Option<u64>,
    payload_sha256: String,
}

#[derive(Clone, Debug)]
struct SourceImport<'a> {
    path: &'a Path,
    source_sha256: &'a str,
    recovery_path: &'a Path,
    migrated_at_ms: u64,
}

/// Inspect whether one legacy source file is absent, pending, current, or has
/// changed since its last successful import. The file is read with the same
/// regular-file and size boundary used by migration.
pub fn migration_status(
    ledger: &RunLedger,
    source_path: impl AsRef<Path>,
) -> ProfileStoreResult<ProfileMigrationStatus> {
    let source_path = source_path.as_ref();
    let stored = load_stored_state(ledger)?;
    let imported_sha256 = stored
        .as_ref()
        .and_then(|state| state.source_sha256.clone());
    let recovery_path = stored
        .as_ref()
        .and_then(|state| state.recovery_path.clone());
    let migrated_at_ms = stored.as_ref().and_then(|state| state.migrated_at_ms);

    match fs::symlink_metadata(source_path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProfileMigrationStatus {
                state: MigrationState::SourceMissing,
                source_path: source_path.to_path_buf(),
                source_sha256: None,
                imported_sha256,
                recovery_path,
                migrated_at_ms,
            });
        }
        Err(source) => return Err(io_error("inspect", source_path, source)),
    }

    let bytes = read_bounded_regular_file(source_path)?;
    let source_sha256 = sha256_hex(&bytes);
    let state = match imported_sha256.as_deref() {
        None => MigrationState::Pending,
        Some(imported) if imported == source_sha256 => MigrationState::Current,
        Some(_) => MigrationState::SourceChanged,
    };
    Ok(ProfileMigrationStatus {
        state,
        source_path: source_path.to_path_buf(),
        source_sha256: Some(source_sha256),
        imported_sha256,
        recovery_path,
        migrated_at_ms,
    })
}

/// Import `chat_sessions.json` into the normalized profile schema. The first
/// valid import writes an exact timestamped recovery copy before any live
/// database row changes. Re-running an identical source is a true no-op.
pub fn migrate_legacy_file(
    ledger: &mut RunLedger,
    artifact_store: &ArtifactStore,
    source_path: impl AsRef<Path>,
) -> ProfileStoreResult<ProfileMigrationResult> {
    let source_path = source_path.as_ref();
    let bytes = read_bounded_regular_file(source_path)?;
    let source_sha256 = sha256_hex(&bytes);
    let prior = load_stored_state(ledger)?;

    if prior
        .as_ref()
        .and_then(|state| state.source_sha256.as_deref())
        == Some(source_sha256.as_str())
    {
        let counts = load_profile_counts(ledger)?;
        return Ok(ProfileMigrationResult {
            outcome: MigrationOutcome::NoChange,
            payload_sha256: prior
                .as_ref()
                .map(|state| state.payload_sha256.clone())
                .unwrap_or_else(|| source_sha256.clone()),
            source_sha256,
            recovery_path: prior.and_then(|state| state.recovery_path),
            counts,
        });
    }

    // Full parse, validation, and extraction precede the recovery copy. A
    // malformed or oversized source therefore cannot partially mutate either
    // artifacts or profile rows; the exact recovery is durable before either
    // store receives data.
    let profile = normalize_payload(&bytes)?;

    let now_ms = now_ms()?;
    let recovery_path = if let Some(path) = prior
        .as_ref()
        .and_then(|state| state.recovery_path.as_ref())
    {
        path.clone()
    } else {
        write_recovery_copy(source_path, &bytes, now_ms, &source_sha256)?
    };
    publish_artifacts(&profile, artifact_store)?;
    let payload_sha256 = source_sha256.clone();
    let source = SourceImport {
        path: source_path,
        source_sha256: &source_sha256,
        recovery_path: &recovery_path,
        migrated_at_ms: now_ms,
    };
    apply_profile(
        ledger,
        artifact_store,
        &profile,
        &payload_sha256,
        Some(source),
        now_ms,
    )?;

    Ok(ProfileMigrationResult {
        outcome: if prior
            .as_ref()
            .and_then(|state| state.source_sha256.as_ref())
            .is_some()
        {
            MigrationOutcome::Updated
        } else {
            MigrationOutcome::Imported
        },
        source_sha256,
        payload_sha256,
        recovery_path: Some(recovery_path),
        counts: profile.counts,
    })
}

/// Transactionally normalize and publish the frontend's current JSON payload.
/// The payload is authoritative: entities omitted after a delete are removed
/// from the profile tables and FTS index in the same transaction. Entities
/// that remain keep deterministic IDs and receive the new values/ordinals.
pub fn save_payload(
    ledger: &mut RunLedger,
    artifact_store: &ArtifactStore,
    payload: &str,
) -> ProfileStoreResult<ProfileSaveResult> {
    if u64::try_from(payload.len()).unwrap_or(u64::MAX) > MAX_PROFILE_JSON_BYTES {
        return Err(ProfileStoreError::InputTooLarge {
            observed: u64::try_from(payload.len()).unwrap_or(u64::MAX),
            max: MAX_PROFILE_JSON_BYTES,
        });
    }
    let bytes = payload.as_bytes();
    let payload_sha256 = sha256_hex(bytes);
    let profile = normalize_payload(bytes)?;
    publish_artifacts(&profile, artifact_store)?;

    let unchanged =
        load_stored_state(ledger)?.is_some_and(|state| state.payload_sha256 == payload_sha256);
    if !unchanged {
        apply_profile(
            ledger,
            artifact_store,
            &profile,
            &payload_sha256,
            None,
            now_ms()?,
        )?;
    }

    Ok(ProfileSaveResult {
        changed: !unchanged,
        payload_sha256,
        counts: profile.counts,
    })
}

/// Runs the exact bounded parser used by [`save_payload`] without publishing
/// artifacts or touching SQLite. Portability restore uses this during its
/// prepare phase so every replacement file is proven valid before the first
/// live profile file is moved aside.
pub fn validate_payload(payload: &str) -> ProfileStoreResult<ProfileCounts> {
    let profile = normalize_payload(payload.as_bytes())?;
    Ok(profile.counts)
}

pub fn global_search(
    ledger: &mut RunLedger,
    request: &GlobalSearchRequest,
) -> ProfileStoreResult<Vec<GlobalSearchHit>> {
    global_search_impl(ledger, None, request, None)
}

/// Search variant used by the desktop/daemon boundary when the shared
/// content-addressed store is available. Newly indexed textual run artifacts
/// contribute their verified bytes; callers without a store retain metadata-
/// only indexing through [`global_search`].
pub fn global_search_with_artifacts(
    ledger: &mut RunLedger,
    artifacts: &ArtifactStore,
    request: &GlobalSearchRequest,
) -> ProfileStoreResult<Vec<GlobalSearchHit>> {
    global_search_impl(ledger, Some(artifacts), request, None)
}

/// Desktop-boundary search variant that limits workspace-owned documents to
/// roots the native process has independently derived from `AppState`.
/// Documents with no workspace remain visible because profile-level content
/// (for example an intentionally global chat) has no root grant to check.
pub fn global_search_with_artifacts_scoped(
    ledger: &mut RunLedger,
    artifacts: &ArtifactStore,
    request: &GlobalSearchRequest,
    allowed_workspace_paths: &[String],
) -> ProfileStoreResult<Vec<GlobalSearchHit>> {
    global_search_impl(
        ledger,
        Some(artifacts),
        request,
        Some(allowed_workspace_paths),
    )
}

fn global_search_impl(
    ledger: &mut RunLedger,
    artifacts: Option<&ArtifactStore>,
    request: &GlobalSearchRequest,
    allowed_workspace_paths: Option<&[String]>,
) -> ProfileStoreResult<Vec<GlobalSearchHit>> {
    validate_search_request(request)?;
    if !ledger_has_fts5(ledger)? {
        return Err(ProfileStoreError::SearchUnavailable);
    }
    sync_run_search_documents(ledger, artifacts)?;

    let fts_query = literal_fts_query(&request.query)?;
    let from_ms = request.from_ms.map(sql_timestamp).transpose()?;
    let to_ms = request.to_ms.map(sql_timestamp).transpose()?;
    let limit = i64::try_from(request.limit)
        .map_err(|_| invalid("search.limit", "cannot be represented by SQLite"))?;
    let mut sql = String::from(
        "SELECT d.document_id, d.source_kind, d.source_id,
                d.session_id, d.run_id, d.title, d.role,
                snippet(profile_search_fts, 0, '[[', ']]', ' … ', 32),
                d.occurred_at_ms, d.model_key, d.persona_id,
                d.workspace_path, d.archived, bm25(profile_search_fts)
           FROM profile_search_fts
           JOIN profile_search_documents d
             ON d.rowid = profile_search_fts.rowid
          WHERE profile_search_fts MATCH ?1
            AND (?2 = 1 OR d.archived = 0)
            AND (?3 IS NULL OR d.occurred_at_ms >= ?3)
            AND (?4 IS NULL OR d.occurred_at_ms <= ?4)
            AND (?5 IS NULL OR d.model_key = ?5)
            AND (?6 IS NULL OR d.persona_id = ?6)
            AND (?7 IS NULL OR d.workspace_path = ?7)",
    );
    if let Some(paths) = allowed_workspace_paths {
        if paths.is_empty() {
            sql.push_str("\n            AND d.workspace_path IS NULL");
        } else {
            let placeholders = (0..paths.len())
                .map(|index| format!("?{}", index + 9))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(
                "\n            AND (d.workspace_path IS NULL OR d.workspace_path IN ({placeholders}))"
            ));
        }
    }
    sql.push_str(
        "\n          ORDER BY bm25(profile_search_fts), d.occurred_at_ms DESC,
                   d.document_id
          LIMIT ?8",
    );

    let mut values = vec![
        SqlValue::Text(fts_query),
        SqlValue::Integer(i64::from(request.include_archived)),
        from_ms.map_or(SqlValue::Null, SqlValue::Integer),
        to_ms.map_or(SqlValue::Null, SqlValue::Integer),
        request
            .model_key
            .clone()
            .map_or(SqlValue::Null, SqlValue::Text),
        request
            .persona_id
            .clone()
            .map_or(SqlValue::Null, SqlValue::Text),
        request
            .workspace_path
            .clone()
            .map_or(SqlValue::Null, SqlValue::Text),
        SqlValue::Integer(limit),
    ];
    if let Some(paths) = allowed_workspace_paths {
        values.extend(paths.iter().cloned().map(SqlValue::Text));
    }

    let mut statement = ledger.connection().prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, Option<String>>(9)?,
            row.get::<_, Option<String>>(10)?,
            row.get::<_, Option<String>>(11)?,
            row.get::<_, i64>(12)?,
            row.get::<_, f64>(13)?,
        ))
    })?;

    let mut hits = Vec::new();
    for row in rows {
        let (
            document_id,
            source_kind,
            source_id,
            session_id,
            run_id,
            title,
            role,
            snippet,
            occurred_at_ms,
            model_key,
            persona_id,
            workspace_path,
            archived,
            score,
        ) = row?;
        hits.push(GlobalSearchHit {
            document_id,
            source_kind: parse_source_kind(&source_kind)?,
            source_id,
            session_id,
            run_id,
            title,
            role,
            snippet: truncate_chars(&snippet, MAX_SEARCH_SNIPPET_CHARS),
            occurred_at_ms: u64::try_from(occurred_at_ms)
                .map_err(|_| ProfileStoreError::Corrupt("negative search timestamp".to_string()))?,
            model_key,
            persona_id,
            workspace_path,
            archived: archived != 0,
            score,
        });
    }
    Ok(hits)
}

fn load_stored_state(ledger: &RunLedger) -> ProfileStoreResult<Option<StoredProfileState>> {
    ledger
        .connection()
        .query_row(
            "SELECT source_sha256, recovery_path, migrated_at_ms, payload_sha256
               FROM profile_state WHERE singleton = 1",
            [],
            |row| {
                let migrated = row.get::<_, Option<i64>>(2)?;
                Ok(StoredProfileState {
                    source_sha256: row.get(0)?,
                    recovery_path: row.get::<_, Option<String>>(1)?.map(PathBuf::from),
                    migrated_at_ms: migrated.and_then(|value| u64::try_from(value).ok()),
                    payload_sha256: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_profile_counts(ledger: &RunLedger) -> ProfileStoreResult<ProfileCounts> {
    let connection = ledger.connection();
    let count = |table: &str| -> ProfileStoreResult<usize> {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        let value = connection.query_row(&sql, [], |row| row.get::<_, i64>(0))?;
        usize::try_from(value)
            .map_err(|_| ProfileStoreError::Corrupt(format!("negative count in {table}")))
    };
    Ok(ProfileCounts {
        groups: count("session_groups")?,
        sessions: count("sessions")?,
        messages: count("messages")?,
        actor_transcripts: count("actor_transcripts")?,
        crews: count("profile_crews")?,
        attachment_occurrences: count("profile_message_attachment_links")?,
        unique_artifacts: count("attachments")?,
    })
}

fn read_bounded_regular_file(path: &Path) -> ProfileStoreResult<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_error("inspect", path, source))?;
    if !metadata.file_type().is_file() {
        return Err(invalid(
            "source",
            format!("{} is not a regular file", path.display()),
        ));
    }
    if metadata.len() > MAX_PROFILE_JSON_BYTES {
        return Err(ProfileStoreError::InputTooLarge {
            observed: metadata.len(),
            max: MAX_PROFILE_JSON_BYTES,
        });
    }

    let file = File::open(path).map_err(|source| io_error("open", path, source))?;
    let opened = file
        .metadata()
        .map_err(|source| io_error("inspect opened", path, source))?;
    if !opened.is_file() || !same_file_identity(&metadata, &opened) {
        return Err(invalid("source", "file changed while being opened"));
    }
    let capacity = usize::try_from(metadata.len())
        .map_err(|_| invalid("source", "file size cannot be represented"))?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_PROFILE_JSON_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read", path, source))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROFILE_JSON_BYTES {
        return Err(ProfileStoreError::InputTooLarge {
            observed: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max: MAX_PROFILE_JSON_BYTES,
        });
    }
    let after = fs::symlink_metadata(path).map_err(|source| io_error("reinspect", path, source))?;
    if !after.file_type().is_file()
        || !same_file_identity(&metadata, &after)
        || bytes.len() != capacity
    {
        return Err(invalid("source", "file changed while being read"));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
}

#[cfg(not(unix))]
fn same_file_identity(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() == after.len() && before.modified().ok() == after.modified().ok()
}

fn write_recovery_copy(
    source_path: &Path,
    bytes: &[u8],
    timestamp_ms: u64,
    digest: &str,
) -> ProfileStoreResult<PathBuf> {
    let parent = source_path
        .parent()
        .ok_or_else(|| invalid("source", "has no parent directory"))?;
    let filename = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("source", "filename is not valid UTF-8"))?;
    let recovery_path = parent.join(format!(
        "{filename}.recovery-{timestamp_ms}-{}.json",
        &digest[..12]
    ));
    let mut recovery = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&recovery_path)
        .map_err(|source| io_error("create recovery copy", &recovery_path, source))?;
    recovery
        .write_all(bytes)
        .map_err(|source| io_error("write recovery copy", &recovery_path, source))?;
    recovery
        .sync_all()
        .map_err(|source| io_error("sync recovery copy", &recovery_path, source))?;
    Ok(recovery_path)
}

fn normalize_payload(bytes: &[u8]) -> ProfileStoreResult<NormalizedProfile> {
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROFILE_JSON_BYTES {
        return Err(ProfileStoreError::InputTooLarge {
            observed: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
            max: MAX_PROFILE_JSON_BYTES,
        });
    }
    let root: Value = serde_json::from_slice(bytes)?;
    let object = root
        .as_object()
        .ok_or_else(|| invalid("$", "must be a JSON object"))?;
    let sessions_value = object
        .get("sessions")
        .ok_or_else(|| invalid("$.sessions", "is required"))?;
    let sessions_array = sessions_value
        .as_array()
        .ok_or_else(|| invalid("$.sessions", "must be an array"))?;
    if sessions_array.is_empty() || sessions_array.len() > MAX_PROFILE_SESSIONS {
        return Err(invalid(
            "$.sessions",
            format!("must contain 1..={MAX_PROFILE_SESSIONS} entries"),
        ));
    }
    let active_session_id = required_id(object, "activeSessionId", "$.activeSessionId")?;
    let groups_array = optional_array(object, "groups", "$.groups")?;
    let crews_array = optional_array(object, "crews", "$.crews")?;
    if groups_array.len() > MAX_PROFILE_GROUPS {
        return Err(invalid(
            "$.groups",
            format!("contains more than {MAX_PROFILE_GROUPS} entries"),
        ));
    }
    if crews_array.len() > MAX_PROFILE_CREWS {
        return Err(invalid(
            "$.crews",
            format!("contains more than {MAX_PROFILE_CREWS} entries"),
        ));
    }

    let mut context = ParseContext::new();
    let mut group_ids = HashSet::new();
    let mut groups = Vec::with_capacity(groups_array.len());
    for (index, raw) in groups_array.iter().enumerate() {
        let path = format!("$.groups[{index}]");
        let group = raw
            .as_object()
            .ok_or_else(|| invalid(&path, "must be an object"))?;
        let id = required_id(group, "id", &format!("{path}.id"))?;
        if !group_ids.insert(id.clone()) {
            return Err(invalid(
                format!("{path}.id"),
                "duplicates an earlier group id",
            ));
        }
        let name =
            required_bounded_string(group, "name", &format!("{path}.name"), MAX_TITLE_BYTES)?;
        let kind = match group.get("kind") {
            None | Some(Value::Null) => "folder".to_string(),
            Some(Value::String(kind)) if kind == "folder" || kind == "comparison" => kind.clone(),
            Some(_) => {
                return Err(invalid(
                    format!("{path}.kind"),
                    "must be 'folder' or 'comparison'",
                ))
            }
        };
        let created_at_ms =
            optional_timestamp(group, "createdAt", &format!("{path}.createdAt"), 1)?;
        let mut metadata = Value::Object(group.clone());
        remove_object_keys(&mut metadata, &["id", "name", "kind", "createdAt"]);
        sanitize_value(&mut metadata, &format!("{path}.metadata"), 0, &mut context)?;
        let metadata_json = serialize_metadata(&metadata, &path, &mut context)?;
        groups.push(NormalizedGroup {
            id,
            ordinal: sql_ordinal(index, &path)?,
            name,
            kind,
            created_at_ms,
            metadata_json,
        });
    }

    let mut session_ids = HashSet::new();
    let mut group_ordinals: HashMap<Option<String>, i64> = HashMap::new();
    let mut sessions = Vec::with_capacity(sessions_array.len());
    for (index, raw) in sessions_array.iter().enumerate() {
        let path = format!("$.sessions[{index}]");
        let session = raw
            .as_object()
            .ok_or_else(|| invalid(&path, "must be an object"))?;
        let id = required_id(session, "id", &format!("{path}.id"))?;
        if !session_ids.insert(id.clone()) {
            return Err(invalid(
                format!("{path}.id"),
                "duplicates an earlier session id",
            ));
        }
        let title =
            required_bounded_string(session, "title", &format!("{path}.title"), MAX_TITLE_BYTES)?;
        let created_at_ms = required_timestamp(session, "createdAt", &format!("{path}.createdAt"))?;
        let updated_at_ms = required_timestamp(session, "updatedAt", &format!("{path}.updatedAt"))?;
        let pinned = optional_bool(session, "pinned", &format!("{path}.pinned"), false)?;
        let unread = optional_bool(session, "unread", &format!("{path}.unread"), false)?;
        let archived = optional_bool(session, "archived", &format!("{path}.archived"), false)?;
        let group_id = optional_nullable_id(session, "groupId", &format!("{path}.groupId"))?;
        if let Some(group_id) = group_id.as_ref() {
            if !group_ids.contains(group_id) {
                return Err(invalid(
                    format!("{path}.groupId"),
                    "does not reference a declared group",
                ));
            }
        }
        let persona_id = optional_nullable_id(session, "personaId", &format!("{path}.personaId"))?;
        let workspace_path = optional_nullable_bounded_string(
            session,
            "workspacePath",
            &format!("{path}.workspacePath"),
            MAX_PATH_BYTES,
        )?;
        let model_key =
            model_key_from_value(session.get("modelTarget"), &format!("{path}.modelTarget"))?;
        let ordinal_entry = group_ordinals.entry(group_id.clone()).or_insert(0);
        let ordinal = *ordinal_entry;
        *ordinal_entry = ordinal_entry
            .checked_add(1)
            .ok_or_else(|| invalid(&path, "session ordinal overflow"))?;

        let raw_messages = session
            .get("messages")
            .ok_or_else(|| invalid(format!("{path}.messages"), "is required"))?
            .as_array()
            .ok_or_else(|| invalid(format!("{path}.messages"), "must be an array"))?;
        context.budget.messages = context
            .budget
            .messages
            .checked_add(raw_messages.len())
            .ok_or_else(|| invalid("$.sessions", "message count overflow"))?;
        if context.budget.messages > MAX_PROFILE_MESSAGES {
            return Err(invalid(
                "$.sessions[*].messages",
                format!("contains more than {MAX_PROFILE_MESSAGES} total messages"),
            ));
        }
        let mut messages = Vec::with_capacity(raw_messages.len());
        for (message_index, raw_message) in raw_messages.iter().enumerate() {
            messages.push(normalize_message(
                raw_message,
                &id,
                message_index,
                created_at_ms,
                updated_at_ms,
                &format!("{path}.messages[{message_index}]"),
                &mut context,
            )?);
        }

        let mut actor_transcripts = Vec::new();
        extract_subagent_transcripts(
            session.get("subagentRuns"),
            &id,
            created_at_ms,
            &persona_id,
            &model_key,
            &path,
            &mut actor_transcripts,
            &mut context,
        )?;
        extract_crew_transcripts(
            session.get("crewRun"),
            &id,
            &workspace_path,
            &path,
            &mut actor_transcripts,
            &mut context,
        )?;

        let mut metadata = Value::Object(session.clone());
        remove_object_keys(
            &mut metadata,
            &[
                "id",
                "title",
                "messages",
                "createdAt",
                "updatedAt",
                "pinned",
                "unread",
                "archived",
                "groupId",
                "workspacePath",
                "personaId",
                "subagentRuns",
            ],
        );
        sanitize_value(&mut metadata, &format!("{path}.metadata"), 0, &mut context)?;
        let metadata_json = serialize_metadata(&metadata, &path, &mut context)?;

        sessions.push(NormalizedSession {
            id,
            group_id,
            ordinal,
            title,
            pinned,
            unread,
            archived,
            created_at_ms,
            updated_at_ms,
            model_key,
            persona_id,
            workspace_path,
            metadata_json,
            messages,
            actor_transcripts,
        });
    }
    if !session_ids.contains(&active_session_id) {
        return Err(invalid(
            "$.activeSessionId",
            "does not reference a declared session",
        ));
    }

    let mut crew_ids = HashSet::new();
    let mut crews = Vec::with_capacity(crews_array.len());
    for (index, raw) in crews_array.iter().enumerate() {
        let path = format!("$.crews[{index}]");
        let crew = raw
            .as_object()
            .ok_or_else(|| invalid(&path, "must be an object"))?;
        let id = required_id(crew, "id", &format!("{path}.id"))?;
        if !crew_ids.insert(id.clone()) {
            return Err(invalid(
                format!("{path}.id"),
                "duplicates an earlier crew id",
            ));
        }
        let name = required_bounded_string(crew, "name", &format!("{path}.name"), MAX_TITLE_BYTES)?;
        let created_at_ms = required_timestamp(crew, "createdAt", &format!("{path}.createdAt"))?;
        let updated_at_ms = required_timestamp(crew, "updatedAt", &format!("{path}.updatedAt"))?;
        let mut metadata = raw.clone();
        sanitize_value(&mut metadata, &format!("{path}.metadata"), 0, &mut context)?;
        let metadata_json = serialize_required_metadata(&metadata, &path, &mut context)?;
        crews.push(NormalizedCrew {
            id,
            ordinal: sql_ordinal(index, &path)?,
            name,
            metadata_json,
            created_at_ms,
            updated_at_ms,
        });
    }

    let mut root_metadata = root.clone();
    remove_object_keys(
        &mut root_metadata,
        &["sessions", "activeSessionId", "groups", "crews"],
    );
    sanitize_value(&mut root_metadata, "$.metadata", 0, &mut context)?;
    let root_metadata_json = serialize_metadata(&root_metadata, "$", &mut context)?;

    let actor_count = sessions
        .iter()
        .map(|session| session.actor_transcripts.len())
        .sum();
    let attachment_occurrences = sessions
        .iter()
        .flat_map(|session| session.messages.iter())
        .map(|message| message.attachments.len())
        .sum();
    let counts = ProfileCounts {
        groups: groups.len(),
        sessions: sessions.len(),
        messages: context.budget.messages,
        actor_transcripts: actor_count,
        crews: crews.len(),
        attachment_occurrences,
        unique_artifacts: context.artifacts.len(),
    };
    Ok(NormalizedProfile {
        active_session_id,
        root_metadata_json,
        groups,
        sessions,
        crews,
        artifacts: context.artifacts,
        attachments: context.attachments,
        counts,
    })
}

#[allow(clippy::too_many_arguments)]
fn normalize_message(
    raw: &Value,
    session_id: &str,
    ordinal: usize,
    session_created_at_ms: i64,
    session_updated_at_ms: i64,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<NormalizedMessage> {
    let message_id = stable_id("msg", &[session_id, &ordinal.to_string()]);
    let created_at_ms = session_created_at_ms
        .checked_add(i64::try_from(ordinal).map_err(|_| invalid(path, "ordinal overflow"))?)
        .unwrap_or(i64::MAX);
    let message_ordinal = sql_ordinal(ordinal, path)?;

    if let Value::String(content) = raw {
        let (content, attachments) = if content.starts_with("data:") {
            let attachment = extract_data_url(content, &format!("{path}.content"), context)?;
            (
                attachment_search_content(&attachment, context),
                vec![MessageAttachmentLink {
                    ordinal: 0,
                    attachment_id: attachment.id,
                    purpose: "legacy_inline_data_url".to_string(),
                }],
            )
        } else {
            if contains_inline_data_url(content) {
                return Err(invalid(
                    format!("{path}.content"),
                    "embedded data URL cannot be safely separated from surrounding text",
                ));
            }
            validate_text(content, &format!("{path}.content"), MAX_MESSAGE_TEXT_BYTES)?;
            (content.clone(), Vec::new())
        };
        return Ok(NormalizedMessage {
            id: message_id,
            ordinal: message_ordinal,
            role: "user".to_string(),
            content,
            metadata_json: None,
            created_at_ms,
            updated_at_ms: session_updated_at_ms,
            attachments,
        });
    }

    let object = raw
        .as_object()
        .ok_or_else(|| invalid(path, "must be an object or legacy string"))?;
    let role = required_bounded_string(object, "role", &format!("{path}.role"), 32)?;
    if !matches!(role.as_str(), "system" | "user" | "assistant" | "tool") {
        return Err(invalid(
            format!("{path}.role"),
            "must be system, user, assistant, or tool",
        ));
    }
    let raw_content = object
        .get("content")
        .ok_or_else(|| invalid(format!("{path}.content"), "is required"))?;
    let mut attachments = Vec::new();
    let mut metadata = Value::Object(object.clone());
    remove_object_keys(&mut metadata, &["role", "content"]);
    let content = match raw_content {
        Value::String(content) => {
            if content.starts_with("data:") {
                let attachment = extract_data_url(content, &format!("{path}.content"), context)?;
                attachments.push(MessageAttachmentLink {
                    ordinal: 0,
                    attachment_id: attachment.id.clone(),
                    purpose: "inline_data_url".to_string(),
                });
                metadata
                    .as_object_mut()
                    .expect("message metadata remains an object")
                    .insert(
                        "contentArtifact".to_string(),
                        artifact_placeholder(&attachment),
                    );
                attachment_search_content(&attachment, context)
            } else {
                if contains_inline_data_url(content) {
                    return Err(invalid(
                        format!("{path}.content"),
                        "embedded data URLs must be represented as image_url content parts",
                    ));
                }
                validate_text(content, &format!("{path}.content"), MAX_MESSAGE_TEXT_BYTES)?;
                content.clone()
            }
        }
        Value::Array(parts) => {
            if parts.len() > MAX_CONTENT_PARTS {
                return Err(invalid(
                    format!("{path}.content"),
                    format!("contains more than {MAX_CONTENT_PARTS} parts"),
                ));
            }
            let mut searchable = Vec::new();
            let mut sanitized_parts = parts.clone();
            for (part_index, part) in sanitized_parts.iter_mut().enumerate() {
                let part_path = format!("{path}.content[{part_index}]");
                let part_object = part
                    .as_object_mut()
                    .ok_or_else(|| invalid(&part_path, "must be an object"))?;
                match part_object.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text =
                            part_object
                                .get("text")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    invalid(format!("{part_path}.text"), "must be a string")
                                })?;
                        validate_text(text, &format!("{part_path}.text"), MAX_MESSAGE_TEXT_BYTES)?;
                        searchable.push(text.to_string());
                    }
                    Some("image_url") => {
                        let image = part_object
                            .get_mut("image_url")
                            .and_then(Value::as_object_mut)
                            .ok_or_else(|| {
                                invalid(format!("{part_path}.image_url"), "must be an object")
                            })?;
                        let url = image
                            .get("url")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                invalid(format!("{part_path}.image_url.url"), "must be a string")
                            })?
                            .to_string();
                        if url.starts_with("data:") {
                            let attachment = extract_data_url(
                                &url,
                                &format!("{part_path}.image_url.url"),
                                context,
                            )?;
                            image.insert("url".to_string(), artifact_placeholder(&attachment));
                            attachments.push(MessageAttachmentLink {
                                ordinal: sql_ordinal(part_index, &part_path)?,
                                attachment_id: attachment.id,
                                purpose: "image_url".to_string(),
                            });
                        } else {
                            validate_text(
                                &url,
                                &format!("{part_path}.image_url.url"),
                                MAX_PATH_BYTES,
                            )?;
                        }
                    }
                    _ => {
                        return Err(invalid(
                            format!("{part_path}.type"),
                            "must be text or image_url",
                        ))
                    }
                }
                sanitize_value(part, &part_path, 0, context)?;
            }
            metadata
                .as_object_mut()
                .expect("message metadata remains an object")
                .insert("contentParts".to_string(), Value::Array(sanitized_parts));
            let text = searchable.join("\n");
            validate_text(&text, &format!("{path}.content"), MAX_MESSAGE_TEXT_BYTES)?;
            text
        }
        _ => {
            return Err(invalid(
                format!("{path}.content"),
                "must be a string or content-part array",
            ))
        }
    };
    sanitize_value(&mut metadata, &format!("{path}.metadata"), 0, context)?;
    let metadata_json = serialize_metadata(&metadata, path, context)?;
    Ok(NormalizedMessage {
        id: message_id,
        ordinal: message_ordinal,
        role,
        content,
        metadata_json,
        created_at_ms,
        updated_at_ms: session_updated_at_ms,
        attachments,
    })
}

#[allow(clippy::too_many_arguments)]
fn extract_subagent_transcripts(
    raw: Option<&Value>,
    session_id: &str,
    session_created_at_ms: i64,
    persona_id: &Option<String>,
    model_key: &Option<String>,
    session_path: &str,
    output: &mut Vec<NormalizedTranscript>,
    context: &mut ParseContext,
) -> ProfileStoreResult<()> {
    let Some(raw) = raw else {
        return Ok(());
    };
    if raw.is_null() {
        return Ok(());
    }
    let runs = raw
        .as_object()
        .ok_or_else(|| invalid(format!("{session_path}.subagentRuns"), "must be an object"))?;
    if runs.len() > MAX_PROFILE_MESSAGES {
        return Err(invalid(
            format!("{session_path}.subagentRuns"),
            "contains too many task transcripts",
        ));
    }
    for (task_id, messages) in runs {
        validate_id(task_id, &format!("{session_path}.subagentRuns key"))?;
        let path = format!("{session_path}.subagentRuns[{task_id:?}]");
        let messages = messages
            .as_array()
            .ok_or_else(|| invalid(&path, "must be an array"))?;
        let actor_id = stable_id("subagent", &[task_id]);
        for (ordinal, message) in messages.iter().enumerate() {
            if output.len() >= MAX_PROFILE_MESSAGES {
                return Err(invalid(
                    format!("{session_path}.subagentRuns"),
                    format!("contains more than {MAX_PROFILE_MESSAGES} transcript entries"),
                ));
            }
            let entry_path = format!("{path}[{ordinal}]");
            let (content, metadata_json) =
                normalize_transcript_message(message, &entry_path, context)?;
            output.push(NormalizedTranscript {
                id: stable_id("transcript", &[session_id, &actor_id, &ordinal.to_string()]),
                actor_id: actor_id.clone(),
                ordinal: sql_ordinal(ordinal, &entry_path)?,
                kind: "subagent".to_string(),
                content,
                created_at_ms: session_created_at_ms
                    .checked_add(i64::try_from(ordinal).unwrap_or(i64::MAX))
                    .unwrap_or(i64::MAX),
                model_key: model_key.clone(),
                persona_id: persona_id.clone(),
                metadata_json,
            });
        }
    }
    Ok(())
}

fn extract_crew_transcripts(
    raw: Option<&Value>,
    session_id: &str,
    _workspace_path: &Option<String>,
    session_path: &str,
    output: &mut Vec<NormalizedTranscript>,
    context: &mut ParseContext,
) -> ProfileStoreResult<()> {
    let Some(raw) = raw else {
        return Ok(());
    };
    if raw.is_null() {
        return Ok(());
    }
    let crew = raw.as_object().ok_or_else(|| {
        invalid(
            format!("{session_path}.crewRun"),
            "must be an object or null",
        )
    })?;
    let coordinator = crew
        .get("coordinator")
        .ok_or_else(|| invalid(format!("{session_path}.crewRun.coordinator"), "is required"))?;
    let members = crew
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            invalid(
                format!("{session_path}.crewRun.members"),
                "must be an array",
            )
        })?;
    if members.len() > 32 {
        return Err(invalid(
            format!("{session_path}.crewRun.members"),
            "contains more than 32 actors",
        ));
    }
    let mut actors = Vec::with_capacity(members.len() + 1);
    actors.push(coordinator);
    actors.extend(members.iter());
    let mut actor_ids = HashSet::new();
    for (actor_index, actor) in actors.into_iter().enumerate() {
        let path = format!("{session_path}.crewRun.actors[{actor_index}]");
        let actor = actor
            .as_object()
            .ok_or_else(|| invalid(&path, "must be an object"))?;
        let actor_id = required_id(actor, "actorId", &format!("{path}.actorId"))?;
        if !actor_ids.insert(actor_id.clone()) {
            return Err(invalid(
                format!("{path}.actorId"),
                "duplicates another crew actor",
            ));
        }
        let model_key =
            model_key_from_value(actor.get("modelTarget"), &format!("{path}.modelTarget"))?;
        let persona_id = match actor.get("persona") {
            None | Some(Value::Null) => None,
            Some(Value::Object(persona)) => {
                Some(required_id(persona, "id", &format!("{path}.persona.id"))?)
            }
            Some(_) => {
                return Err(invalid(
                    format!("{path}.persona"),
                    "must be an object or null",
                ))
            }
        };
        let transcript = actor
            .get("transcript")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid(format!("{path}.transcript"), "must be an array"))?;
        let mut entry_ids = HashSet::new();
        for (ordinal, entry) in transcript.iter().enumerate() {
            if output.len() >= MAX_PROFILE_MESSAGES {
                return Err(invalid(
                    format!("{session_path}.crewRun"),
                    format!("contains more than {MAX_PROFILE_MESSAGES} transcript entries"),
                ));
            }
            let entry_path = format!("{path}.transcript[{ordinal}]");
            let entry = entry
                .as_object()
                .ok_or_else(|| invalid(&entry_path, "must be an object"))?;
            let entry_id = required_id(entry, "id", &format!("{entry_path}.id"))?;
            if !entry_ids.insert(entry_id.clone()) {
                return Err(invalid(
                    format!("{entry_path}.id"),
                    "duplicates an earlier entry",
                ));
            }
            if let Some(entry_actor) = entry.get("actorId") {
                if entry_actor.as_str() != Some(actor_id.as_str()) {
                    return Err(invalid(
                        format!("{entry_path}.actorId"),
                        "does not match the owning actor",
                    ));
                }
            }
            let kind = required_bounded_string(entry, "kind", &format!("{entry_path}.kind"), 32)?;
            if !matches!(
                kind.as_str(),
                "model" | "tool_request" | "tool_result" | "notice"
            ) {
                return Err(invalid(
                    format!("{entry_path}.kind"),
                    "must be model, tool_request, tool_result, or notice",
                ));
            }
            let raw_content = required_bounded_string(
                entry,
                "content",
                &format!("{entry_path}.content"),
                MAX_MESSAGE_TEXT_BYTES,
            )?;
            let content =
                normalize_data_text(&raw_content, &format!("{entry_path}.content"), context)?;
            let created_at_ms = required_timestamp(entry, "at", &format!("{entry_path}.at"))?;
            let mut metadata = Value::Object(entry.clone());
            remove_object_keys(&mut metadata, &["id", "actorId", "at", "kind", "content"]);
            sanitize_value(&mut metadata, &format!("{entry_path}.metadata"), 0, context)?;
            let metadata_json = serialize_metadata(&metadata, &entry_path, context)?;
            output.push(NormalizedTranscript {
                id: stable_id("transcript", &[session_id, &actor_id, &entry_id]),
                actor_id: actor_id.clone(),
                ordinal: sql_ordinal(ordinal, &entry_path)?,
                kind,
                content,
                created_at_ms,
                model_key: model_key.clone(),
                persona_id: persona_id.clone(),
                metadata_json,
            });
        }
    }
    Ok(())
}

fn normalize_transcript_message(
    raw: &Value,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<(String, Option<Vec<u8>>)> {
    if let Value::String(content) = raw {
        let content = normalize_data_text(content, path, context)?;
        return Ok((content, None));
    }
    let message = raw
        .as_object()
        .ok_or_else(|| invalid(path, "must be a message object or legacy string"))?;
    let role = required_bounded_string(message, "role", &format!("{path}.role"), 32)?;
    if !matches!(role.as_str(), "system" | "user" | "assistant" | "tool") {
        return Err(invalid(
            format!("{path}.role"),
            "must be system, user, assistant, or tool",
        ));
    }
    let raw_content = message
        .get("content")
        .ok_or_else(|| invalid(format!("{path}.content"), "is required"))?;
    let content = match raw_content {
        Value::String(content) => {
            normalize_data_text(content, &format!("{path}.content"), context)?
        }
        Value::Array(parts) => {
            if parts.len() > MAX_CONTENT_PARTS {
                return Err(invalid(
                    format!("{path}.content"),
                    format!("contains more than {MAX_CONTENT_PARTS} parts"),
                ));
            }
            let mut text = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let part = part.as_object().ok_or_else(|| {
                    invalid(format!("{path}.content[{index}]"), "must be an object")
                })?;
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    let value = part.get("text").and_then(Value::as_str).ok_or_else(|| {
                        invalid(format!("{path}.content[{index}].text"), "must be a string")
                    })?;
                    validate_text(
                        value,
                        &format!("{path}.content[{index}].text"),
                        MAX_MESSAGE_TEXT_BYTES,
                    )?;
                    text.push(value.to_string());
                } else if part.get("type").and_then(Value::as_str) != Some("image_url") {
                    return Err(invalid(
                        format!("{path}.content[{index}].type"),
                        "must be text or image_url",
                    ));
                }
            }
            let text = text.join("\n");
            validate_text(&text, &format!("{path}.content"), MAX_MESSAGE_TEXT_BYTES)?;
            text
        }
        _ => {
            return Err(invalid(
                format!("{path}.content"),
                "must be a string or content-part array",
            ))
        }
    };
    let mut metadata = raw.clone();
    sanitize_value(&mut metadata, &format!("{path}.metadata"), 0, context)?;
    let metadata_json = serialize_metadata(&metadata, path, context)?;
    Ok((content, metadata_json))
}

fn sanitize_value(
    value: &mut Value,
    path: &str,
    depth: usize,
    context: &mut ParseContext,
) -> ProfileStoreResult<()> {
    if depth > 128 {
        return Err(invalid(path, "JSON nesting exceeds 128 levels"));
    }
    context.budget.json_nodes = context
        .budget
        .json_nodes
        .checked_add(1)
        .ok_or_else(|| invalid(path, "JSON node count overflow"))?;
    if context.budget.json_nodes > MAX_JSON_NODES {
        return Err(invalid(
            path,
            format!("metadata contains more than {MAX_JSON_NODES} JSON nodes"),
        ));
    }

    match value {
        Value::String(string) if string.starts_with("data:") => {
            let attachment = extract_data_url(string, path, context)?;
            *value = artifact_placeholder(&attachment);
        }
        Value::String(string) => {
            if contains_inline_data_url(string) {
                return Err(invalid(
                    path,
                    "embedded data URL cannot be safely separated from surrounding text",
                ));
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                sanitize_value(value, &format!("{path}[{index}]"), depth + 1, context)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values.iter_mut() {
                sanitize_value(value, &format!("{path}.{key}"), depth + 1, context)?;
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn extract_data_url(
    value: &str,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<NormalizedAttachment> {
    let body = value
        .strip_prefix("data:")
        .ok_or_else(|| invalid(path, "is not a data URL"))?;
    let (header, encoded) = body
        .split_once(',')
        .ok_or_else(|| invalid(path, "data URL is missing its comma separator"))?;
    let mut segments = header.split(';');
    let media_type = segments.next().unwrap_or_default();
    let media_type = if media_type.is_empty() {
        "application/octet-stream"
    } else {
        media_type
    };
    validate_media_type(media_type, path)?;
    let parameters = segments.collect::<Vec<_>>();
    if parameters.len() != 1 || !parameters[0].eq_ignore_ascii_case("base64") {
        return Err(invalid(
            path,
            "only canonical base64 data URLs are accepted",
        ));
    }
    let estimated = encoded
        .len()
        .checked_add(3)
        .and_then(|size| size.checked_div(4))
        .and_then(|size| size.checked_mul(3))
        .ok_or_else(|| invalid(path, "encoded size overflow"))?;
    if u64::try_from(estimated).unwrap_or(u64::MAX) > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(invalid(path, "data URL exceeds the attachment byte budget"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| invalid(path, format!("invalid base64 data: {error}")))?;
    let byte_size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    context.budget.attachment_bytes = context
        .budget
        .attachment_bytes
        .checked_add(byte_size)
        .ok_or_else(|| invalid(path, "attachment byte count overflow"))?;
    if context.budget.attachment_bytes > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(invalid(
            path,
            format!("total decoded attachments exceed {MAX_TOTAL_ATTACHMENT_BYTES} bytes"),
        ));
    }
    let blob_id = sha256_hex(&bytes);
    let attachment_id = stable_id("att", &[&blob_id, media_type]);
    context
        .artifacts
        .entry(blob_id.clone())
        .or_insert(PendingArtifact { bytes });
    let attachment = NormalizedAttachment {
        id: attachment_id.clone(),
        blob_id,
        media_type: media_type.to_ascii_lowercase(),
        byte_size: i64::try_from(byte_size)
            .map_err(|_| invalid(path, "decoded byte count exceeds SQLite range"))?,
    };
    context
        .attachments
        .entry(attachment_id)
        .or_insert_with(|| attachment.clone());
    Ok(attachment)
}

fn artifact_placeholder(attachment: &NormalizedAttachment) -> Value {
    json!({
        "$littleMonkeyArtifact": {
            "attachmentId": attachment.id,
            "contentSha256": attachment.blob_id,
            "mediaType": attachment.media_type,
            "byteSize": attachment.byte_size,
        }
    })
}

fn attachment_search_content(attachment: &NormalizedAttachment, context: &ParseContext) -> String {
    let marker = format!(
        "[Attachment: {} {}]",
        attachment.media_type, attachment.blob_id
    );
    let media = attachment.media_type.split(';').next().unwrap_or_default();
    let textual = media.starts_with("text/")
        || matches!(
            media,
            "application/json"
                | "application/ld+json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
        )
        || media.ends_with("+json")
        || media.ends_with("+xml");
    if !textual {
        return marker;
    }
    let Some(pending) = context.artifacts.get(&attachment.blob_id) else {
        return marker;
    };
    if pending.bytes.len() > MAX_MESSAGE_TEXT_BYTES {
        return marker;
    }
    let Ok(text) = std::str::from_utf8(&pending.bytes) else {
        return marker;
    };
    if text.contains('\0') {
        return marker;
    }
    format!("{marker}\n{text}")
}

fn normalize_data_text(
    content: &str,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<String> {
    if content.starts_with("data:") {
        let attachment = extract_data_url(content, path, context)?;
        Ok(attachment_search_content(&attachment, context))
    } else if contains_inline_data_url(content) {
        Err(invalid(
            path,
            "embedded data URL cannot be safely separated from surrounding text",
        ))
    } else {
        validate_text(content, path, MAX_MESSAGE_TEXT_BYTES)?;
        Ok(content.to_string())
    }
}

fn contains_inline_data_url(value: &str) -> bool {
    value.contains("data:") && value.contains(";base64,")
}

fn serialize_metadata(
    value: &Value,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<Option<Vec<u8>>> {
    let empty = matches!(value, Value::Object(object) if object.is_empty());
    if empty {
        return Ok(None);
    }
    serialize_required_metadata(value, path, context).map(Some)
}

fn serialize_required_metadata(
    value: &Value,
    path: &str,
    context: &mut ParseContext,
) -> ProfileStoreResult<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_ENTITY_METADATA_BYTES {
        return Err(invalid(
            format!("{path}.metadata"),
            format!("exceeds {MAX_ENTITY_METADATA_BYTES} bytes"),
        ));
    }
    context.budget.metadata_bytes = context
        .budget
        .metadata_bytes
        .checked_add(bytes.len())
        .ok_or_else(|| invalid(path, "metadata byte count overflow"))?;
    if context.budget.metadata_bytes > MAX_TOTAL_METADATA_BYTES {
        return Err(invalid(
            "$.metadata",
            format!("exceeds {MAX_TOTAL_METADATA_BYTES} total bytes"),
        ));
    }
    Ok(bytes)
}

fn required_id(object: &Map<String, Value>, key: &str, path: &str) -> ProfileStoreResult<String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, "must be a string"))?;
    validate_id(value, path)?;
    Ok(value.to_string())
}

fn validate_id(value: &str, path: &str) -> ProfileStoreResult<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(invalid(
            path,
            format!("must contain 1..={MAX_ID_BYTES} UTF-8 bytes"),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(path, "must not contain control characters"));
    }
    Ok(())
}

fn required_bounded_string(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    max_bytes: usize,
) -> ProfileStoreResult<String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(path, "must be a string"))?;
    validate_text(value, path, max_bytes)?;
    Ok(value.to_string())
}

fn optional_nullable_id(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> ProfileStoreResult<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            validate_id(value, path)?;
            Ok(Some(value.clone()))
        }
        Some(_) => Err(invalid(path, "must be a string or null")),
    }
}

fn optional_nullable_bounded_string(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    max_bytes: usize,
) -> ProfileStoreResult<Option<String>> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => {
            validate_text(value, path, max_bytes)?;
            Ok(Some(value.clone()))
        }
        Some(_) => Err(invalid(path, "must be a string or null")),
    }
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    default: bool,
) -> ProfileStoreResult<bool> {
    match object.get(key) {
        None => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(invalid(path, "must be a boolean")),
    }
}

fn required_timestamp(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> ProfileStoreResult<i64> {
    let value = object
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(path, "must be a non-negative integer timestamp"))?;
    // Older frontend normalizers used zero as their recovery default. SQLite
    // reserves positive timestamps, so that exact legacy sentinel becomes 1.
    sql_timestamp(value.max(1))
}

fn optional_timestamp(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
    default: u64,
) -> ProfileStoreResult<i64> {
    match object.get(key) {
        None | Some(Value::Null) => sql_timestamp(default.max(1)),
        Some(Value::Number(number)) => {
            let value = number
                .as_u64()
                .ok_or_else(|| invalid(path, "must be a non-negative integer timestamp"))?;
            sql_timestamp(value.max(1))
        }
        Some(_) => Err(invalid(path, "must be a non-negative integer timestamp")),
    }
}

fn optional_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> ProfileStoreResult<&'a [Value]> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(invalid(path, "must be an array")),
    }
}

fn model_key_from_value(value: Option<&Value>, path: &str) -> ProfileStoreResult<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(target)) => {
            let key = required_bounded_string(target, "key", &format!("{path}.key"), MAX_ID_BYTES)?;
            Ok(Some(key))
        }
        Some(_) => Err(invalid(path, "must be an object or null")),
    }
}

fn validate_text(value: &str, path: &str, max_bytes: usize) -> ProfileStoreResult<()> {
    if value.len() > max_bytes {
        return Err(invalid(path, format!("exceeds {max_bytes} UTF-8 bytes")));
    }
    if value.contains('\0') {
        return Err(invalid(path, "must not contain NUL"));
    }
    Ok(())
}

fn validate_media_type(value: &str, path: &str) -> ProfileStoreResult<()> {
    if value.len() > 255
        || !value.is_ascii()
        || !value.contains('/')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(invalid(path, "contains an invalid media type"));
    }
    Ok(())
}

fn remove_object_keys(value: &mut Value, keys: &[&str]) {
    if let Value::Object(object) = value {
        for key in keys {
            object.remove(*key);
        }
    }
}

fn publish_artifacts(
    profile: &NormalizedProfile,
    artifact_store: &ArtifactStore,
) -> ProfileStoreResult<()> {
    for (expected_id, pending) in &profile.artifacts {
        let observed = u64::try_from(pending.bytes.len()).unwrap_or(u64::MAX);
        if observed > artifact_store.max_blob_size() {
            return Err(invalid(
                "$.attachments",
                format!(
                    "artifact {expected_id} is {observed} bytes, exceeding the store's {} byte limit",
                    artifact_store.max_blob_size()
                ),
            ));
        }
    }
    for (expected_id, pending) in &profile.artifacts {
        let observed = u64::try_from(pending.bytes.len()).unwrap_or(u64::MAX);
        let blob = artifact_store.put(&pending.bytes)?;
        if blob.id != *expected_id || blob.size != observed {
            return Err(ProfileStoreError::Corrupt(format!(
                "artifact store returned {}:{} for expected {expected_id}:{observed}",
                blob.id, blob.size
            )));
        }
    }
    Ok(())
}

fn apply_profile(
    ledger: &mut RunLedger,
    artifact_store: &ArtifactStore,
    profile: &NormalizedProfile,
    payload_sha256: &str,
    source: Option<SourceImport<'_>>,
    saved_at_ms: u64,
) -> ProfileStoreResult<()> {
    let saved_at_ms = sql_timestamp(saved_at_ms)?;
    let transaction = ledger
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;

    // Materialize the authoritative id set in connection-local temporary
    // tables. This avoids SQLite's host-parameter limit for large profiles
    // and lets every deletion stay inside the same atomic publication.
    for (table, column) in [
        ("desired_profile_groups", "id"),
        ("desired_profile_sessions", "id"),
        ("desired_profile_messages", "id"),
        ("desired_profile_transcripts", "id"),
        ("desired_profile_crews", "id"),
    ] {
        transaction.execute_batch(&format!(
            "CREATE TEMP TABLE IF NOT EXISTS {table} ({column} TEXT PRIMARY KEY) WITHOUT ROWID;
             DELETE FROM {table};"
        ))?;
    }
    for group in &profile.groups {
        transaction.execute(
            "INSERT INTO desired_profile_groups(id) VALUES (?1)",
            [&group.id],
        )?;
    }
    for session in &profile.sessions {
        transaction.execute(
            "INSERT INTO desired_profile_sessions(id) VALUES (?1)",
            [&session.id],
        )?;
        for message in &session.messages {
            transaction.execute(
                "INSERT INTO desired_profile_messages(id) VALUES (?1)",
                [&message.id],
            )?;
        }
        for transcript in &session.actor_transcripts {
            transaction.execute(
                "INSERT INTO desired_profile_transcripts(id) VALUES (?1)",
                [&transcript.id],
            )?;
        }
    }
    for crew in &profile.crews {
        transaction.execute(
            "INSERT INTO desired_profile_crews(id) VALUES (?1)",
            [&crew.id],
        )?;
    }

    // Search rows must disappear before their RESTRICTed session/message
    // parents. Run-event documents are deliberately outside this profile
    // snapshot and therefore remain untouched.
    transaction.execute_batch(
        "DELETE FROM profile_search_documents
           WHERE source_kind = 'message'
             AND source_id NOT IN (SELECT id FROM desired_profile_messages);
         DELETE FROM profile_search_documents
           WHERE source_kind = 'actor_transcript'
             AND source_id NOT IN (SELECT id FROM desired_profile_transcripts);
         DELETE FROM message_translations
           WHERE message_id NOT IN (SELECT id FROM desired_profile_messages);
         DELETE FROM message_attachments
           WHERE message_id NOT IN (SELECT id FROM desired_profile_messages);
         DELETE FROM profile_message_attachment_links
           WHERE message_id NOT IN (SELECT id FROM desired_profile_messages);
         DELETE FROM actor_transcripts
           WHERE transcript_id NOT IN (SELECT id FROM desired_profile_transcripts);
         DELETE FROM messages
           WHERE message_id NOT IN (SELECT id FROM desired_profile_messages);",
    )?;

    // Move existing ordinals out of the compact live range before upserts so
    // arbitrary reorder/move operations cannot trip unique constraints.
    transaction.execute(
        "UPDATE session_groups SET ordinal = ordinal + ?1",
        [ORDINAL_SHIFT],
    )?;
    transaction.execute(
        "UPDATE sessions SET ordinal = ordinal + ?1",
        [ORDINAL_SHIFT],
    )?;
    transaction.execute(
        "UPDATE profile_crews SET ordinal = ordinal + ?1",
        [ORDINAL_SHIFT],
    )?;
    for session in &profile.sessions {
        transaction.execute(
            "UPDATE messages SET ordinal = ordinal + ?1 WHERE session_id = ?2",
            params![ORDINAL_SHIFT, session.id],
        )?;
        transaction.execute(
            "UPDATE actor_transcripts SET ordinal = ordinal + ?1 WHERE session_id = ?2",
            params![ORDINAL_SHIFT, session.id],
        )?;
    }

    for group in &profile.groups {
        transaction.execute(
            "INSERT INTO session_groups (
                group_id, parent_group_id, ordinal, name, created_at_ms,
                updated_at_ms, kind, metadata_json
             ) VALUES (?1, NULL, ?2, ?3, ?4, ?4, ?5, ?6)
             ON CONFLICT(group_id) DO UPDATE SET
                parent_group_id = excluded.parent_group_id,
                ordinal = excluded.ordinal,
                name = excluded.name,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms,
                kind = excluded.kind,
                metadata_json = excluded.metadata_json",
            params![
                group.id,
                group.ordinal,
                group.name,
                group.created_at_ms,
                group.kind,
                group.metadata_json,
            ],
        )?;
    }

    for attachment in profile.attachments.values() {
        let storage_path = artifact_store.blob_path(&attachment.blob_id)?;
        let storage_path = storage_path
            .to_str()
            .ok_or_else(|| invalid("artifact.storagePath", "is not valid UTF-8"))?;
        transaction.execute(
            "INSERT INTO attachments (
                attachment_id, content_sha256, kind, media_type, byte_size,
                storage_path, metadata_json, created_at_ms
             ) VALUES (?1, ?2, 'profile_data_url', ?3, ?4, ?5, NULL, ?6)
             ON CONFLICT(attachment_id) DO UPDATE SET
                content_sha256 = excluded.content_sha256,
                kind = excluded.kind,
                media_type = excluded.media_type,
                byte_size = excluded.byte_size,
                storage_path = excluded.storage_path",
            params![
                attachment.id,
                attachment.blob_id,
                attachment.media_type,
                attachment.byte_size,
                storage_path,
                saved_at_ms,
            ],
        )?;
    }

    for session in &profile.sessions {
        transaction.execute(
            "INSERT INTO sessions (
                session_id, group_id, ordinal, title, active_run_id, pinned,
                archived, created_at_ms, updated_at_ms, unread, model_key,
                persona_id, workspace_path, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(session_id) DO UPDATE SET
                group_id = excluded.group_id,
                ordinal = excluded.ordinal,
                title = excluded.title,
                pinned = excluded.pinned,
                archived = excluded.archived,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms,
                unread = excluded.unread,
                model_key = excluded.model_key,
                persona_id = excluded.persona_id,
                workspace_path = excluded.workspace_path,
                metadata_json = excluded.metadata_json",
            params![
                session.id,
                session.group_id,
                session.ordinal,
                session.title,
                i64::from(session.pinned),
                i64::from(session.archived),
                session.created_at_ms,
                session.updated_at_ms,
                i64::from(session.unread),
                session.model_key,
                session.persona_id,
                session.workspace_path,
                session.metadata_json,
            ],
        )?;

        // Rows retained from an earlier payload inherit current session-level
        // filters, even when the new payload no longer lists that message.
        transaction.execute(
            "UPDATE profile_search_documents
                SET title = ?2, model_key = ?3, persona_id = ?4,
                    workspace_path = ?5, archived = ?6
              WHERE session_id = ?1",
            params![
                session.id,
                session.title,
                session.model_key,
                session.persona_id,
                session.workspace_path,
                i64::from(session.archived),
            ],
        )?;

        for message in &session.messages {
            transaction.execute(
                "INSERT INTO messages (
                    message_id, session_id, ordinal, run_id, actor_id, role,
                    content, metadata_json, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(message_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    ordinal = excluded.ordinal,
                    role = excluded.role,
                    content = excluded.content,
                    metadata_json = excluded.metadata_json,
                    created_at_ms = excluded.created_at_ms,
                    updated_at_ms = excluded.updated_at_ms",
                params![
                    message.id,
                    session.id,
                    message.ordinal,
                    message.role,
                    message.content,
                    message.metadata_json,
                    message.created_at_ms,
                    message.updated_at_ms,
                ],
            )?;
            for link in &message.attachments {
                transaction.execute(
                    "INSERT INTO profile_message_attachment_links (
                        message_id, ordinal, attachment_id, purpose
                     ) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(message_id, ordinal) DO UPDATE SET
                        attachment_id = excluded.attachment_id,
                        purpose = excluded.purpose",
                    params![message.id, link.ordinal, link.attachment_id, link.purpose],
                )?;
            }
            upsert_search_document(
                &transaction,
                &stable_id("search-message", &[&message.id]),
                "message",
                &message.id,
                Some(&session.id),
                None,
                &session.title,
                &message.role,
                &message.content,
                message.created_at_ms,
                session.model_key.as_deref(),
                session.persona_id.as_deref(),
                session.workspace_path.as_deref(),
                session.archived,
                None,
            )?;
        }

        for transcript in &session.actor_transcripts {
            transaction.execute(
                "INSERT INTO actor_transcripts (
                    transcript_id, session_id, actor_id, ordinal, run_id,
                    message_id, content, created_at_ms, kind, model_key,
                    persona_id, workspace_path, metadata_json
                 ) VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                 ON CONFLICT(transcript_id) DO UPDATE SET
                    session_id = excluded.session_id,
                    actor_id = excluded.actor_id,
                    ordinal = excluded.ordinal,
                    content = excluded.content,
                    created_at_ms = excluded.created_at_ms,
                    kind = excluded.kind,
                    model_key = excluded.model_key,
                    persona_id = excluded.persona_id,
                    workspace_path = excluded.workspace_path,
                    metadata_json = excluded.metadata_json",
                params![
                    transcript.id,
                    session.id,
                    transcript.actor_id,
                    transcript.ordinal,
                    transcript.content,
                    transcript.created_at_ms,
                    transcript.kind,
                    transcript.model_key,
                    transcript.persona_id,
                    session.workspace_path,
                    transcript.metadata_json,
                ],
            )?;
            upsert_search_document(
                &transaction,
                &stable_id("search-transcript", &[&transcript.id]),
                "actor_transcript",
                &transcript.id,
                Some(&session.id),
                None,
                &session.title,
                &transcript.kind,
                &transcript.content,
                transcript.created_at_ms,
                transcript.model_key.as_deref(),
                transcript.persona_id.as_deref(),
                session.workspace_path.as_deref(),
                session.archived,
                transcript.metadata_json.as_deref(),
            )?;
        }
    }

    for crew in &profile.crews {
        transaction.execute(
            "INSERT INTO profile_crews (
                crew_id, ordinal, name, metadata_json, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(crew_id) DO UPDATE SET
                ordinal = excluded.ordinal,
                name = excluded.name,
                metadata_json = excluded.metadata_json,
                created_at_ms = excluded.created_at_ms,
                updated_at_ms = excluded.updated_at_ms",
            params![
                crew.id,
                crew.ordinal,
                crew.name,
                crew.metadata_json,
                crew.created_at_ms,
                crew.updated_at_ms,
            ],
        )?;
    }

    let (source_path, source_sha256, recovery_path, migrated_at_ms) = match source {
        Some(source) => (
            Some(
                source
                    .path
                    .to_str()
                    .ok_or_else(|| invalid("source", "path is not valid UTF-8"))?,
            ),
            Some(source.source_sha256),
            Some(
                source
                    .recovery_path
                    .to_str()
                    .ok_or_else(|| invalid("recovery", "path is not valid UTF-8"))?,
            ),
            Some(sql_timestamp(source.migrated_at_ms)?),
        ),
        None => (None, None, None, None),
    };
    transaction.execute(
        "INSERT INTO profile_state (
            singleton, source_path, source_sha256, recovery_path,
            migrated_at_ms, payload_sha256, active_session_id,
            root_metadata_json, saved_at_ms, last_indexed_run_event_rowid
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
         ON CONFLICT(singleton) DO UPDATE SET
            source_path = COALESCE(excluded.source_path, profile_state.source_path),
            source_sha256 = COALESCE(excluded.source_sha256, profile_state.source_sha256),
            recovery_path = COALESCE(excluded.recovery_path, profile_state.recovery_path),
            migrated_at_ms = COALESCE(excluded.migrated_at_ms, profile_state.migrated_at_ms),
            payload_sha256 = excluded.payload_sha256,
            active_session_id = excluded.active_session_id,
            root_metadata_json = excluded.root_metadata_json,
            saved_at_ms = excluded.saved_at_ms",
        params![
            source_path,
            source_sha256,
            recovery_path,
            migrated_at_ms,
            payload_sha256,
            profile.active_session_id,
            profile.root_metadata_json,
            saved_at_ms,
        ],
    )?;

    // The state row now points at the new active session, so obsolete
    // sessions can be removed without violating its RESTRICT constraint.
    transaction.execute_batch(
        "DELETE FROM actor_transcripts
           WHERE session_id NOT IN (SELECT id FROM desired_profile_sessions);
         DELETE FROM messages
           WHERE session_id NOT IN (SELECT id FROM desired_profile_sessions);
         DELETE FROM sessions
           WHERE session_id NOT IN (SELECT id FROM desired_profile_sessions);
         DELETE FROM session_groups
           WHERE group_id NOT IN (SELECT id FROM desired_profile_groups);
         DELETE FROM profile_crews
           WHERE crew_id NOT IN (SELECT id FROM desired_profile_crews);
         DROP TABLE desired_profile_groups;
         DROP TABLE desired_profile_sessions;
         DROP TABLE desired_profile_messages;
         DROP TABLE desired_profile_transcripts;
         DROP TABLE desired_profile_crews;",
    )?;
    transaction.commit()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn upsert_search_document(
    transaction: &Transaction<'_>,
    document_id: &str,
    source_kind: &str,
    source_id: &str,
    session_id: Option<&str>,
    run_id: Option<&str>,
    title: &str,
    role: &str,
    content: &str,
    occurred_at_ms: i64,
    model_key: Option<&str>,
    persona_id: Option<&str>,
    workspace_path: Option<&str>,
    archived: bool,
    metadata_json: Option<&[u8]>,
) -> ProfileStoreResult<()> {
    transaction.execute(
        "INSERT INTO profile_search_documents (
            document_id, source_kind, source_id, session_id, run_id, title,
            role, content, occurred_at_ms, model_key, persona_id,
            workspace_path, archived, metadata_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(document_id) DO UPDATE SET
            source_kind = excluded.source_kind,
            source_id = excluded.source_id,
            session_id = excluded.session_id,
            run_id = excluded.run_id,
            title = excluded.title,
            role = excluded.role,
            content = excluded.content,
            occurred_at_ms = excluded.occurred_at_ms,
            model_key = excluded.model_key,
            persona_id = excluded.persona_id,
            workspace_path = excluded.workspace_path,
            archived = excluded.archived,
            metadata_json = excluded.metadata_json",
        params![
            document_id,
            source_kind,
            source_id,
            session_id,
            run_id,
            title,
            role,
            content,
            occurred_at_ms,
            model_key,
            persona_id,
            workspace_path,
            i64::from(archived),
            metadata_json,
        ],
    )?;
    Ok(())
}

fn sync_run_search_documents(
    ledger: &mut RunLedger,
    artifacts: Option<&ArtifactStore>,
) -> ProfileStoreResult<()> {
    let transaction = ledger
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    let last_rowid = transaction.query_row(
        "SELECT last_indexed_run_event_rowid
           FROM profile_run_search_state WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    let mut indexed_through = last_rowid;
    loop {
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT e.rowid, e.envelope_json, r.spec_json
                   FROM run_events e
                   JOIN runs r ON r.run_id = e.run_id
                  WHERE e.rowid > ?1
                  ORDER BY e.rowid
                  LIMIT 10000",
            )?;
            let mapped = statement.query_map([indexed_through], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            break;
        }
        for (rowid, envelope_json, spec_json) in rows {
            let envelope: RunEventEnvelope =
                serde_json::from_slice(&envelope_json).map_err(|error| {
                    ProfileStoreError::Corrupt(format!("invalid run event JSON: {error}"))
                })?;
            envelope.validate().map_err(|error| {
                ProfileStoreError::Corrupt(format!(
                    "invalid run event {}: {error}",
                    envelope.event_id
                ))
            })?;
            let spec: RunSpec = serde_json::from_slice(&spec_json).map_err(|error| {
                ProfileStoreError::Corrupt(format!("invalid run spec JSON: {error}"))
            })?;
            spec.validate().map_err(|error| {
                ProfileStoreError::Corrupt(format!("invalid run spec {}: {error}", spec.run_id))
            })?;
            if spec.run_id != envelope.run_id {
                return Err(ProfileStoreError::Corrupt(format!(
                    "event {} belongs to {}, joined to {}",
                    envelope.event_id, envelope.run_id, spec.run_id
                )));
            }

            let (event_kind, role, content) =
                searchable_run_event(&envelope.event, &spec, artifacts);
            if !content.trim().is_empty() {
                let workspace_path = spec.workspace.as_ref().and_then(|workspace| {
                    workspace
                        .roots
                        .iter()
                        .find(|root| root.root_id == workspace.primary_root_id)
                        .map(|root| root.canonical_path.as_str())
                });
                let metadata = serde_json::to_vec(&json!({
                    "eventType": event_kind,
                    "sequence": envelope.sequence,
                }))?;
                upsert_search_document(
                    &transaction,
                    &stable_id("search-run-event", &[&envelope.event_id]),
                    "run_event",
                    &envelope.event_id,
                    None,
                    Some(&envelope.run_id),
                    &spec.task,
                    role,
                    &content,
                    sql_timestamp(envelope.occurred_at_ms)?,
                    Some(spec.target.target_id()),
                    None,
                    workspace_path,
                    false,
                    Some(&metadata),
                )?;
            }
            indexed_through = rowid;
        }
    }
    if indexed_through != last_rowid {
        transaction.execute(
            "UPDATE profile_run_search_state
                SET last_indexed_run_event_rowid = ?1
              WHERE singleton = 1",
            [indexed_through],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn searchable_run_event(
    event: &RunEvent,
    spec: &RunSpec,
    artifacts: Option<&ArtifactStore>,
) -> (&'static str, &'static str, String) {
    match event {
        RunEvent::Queued { queue } => (
            "queued",
            "run",
            join_search_text([
                Some(spec.task.as_str()),
                spec.instructions.as_deref(),
                queue.as_deref(),
            ]),
        ),
        RunEvent::Started { engine_id } => ("started", "run", engine_id.clone()),
        // Searchable by the reason, which is the sentence naming the policy —
        // "which policy chose this run's target" is the question this event
        // exists to answer, so it is the text worth finding it by.
        RunEvent::RoutingDecided {
            policy_name,
            reason,
            ..
        } => (
            "routing_decided",
            "run",
            join_search_text([policy_name.as_deref(), Some(reason.as_str())]),
        ),
        RunEvent::ModelDelta { channel, text, .. } => (
            "model_delta",
            match channel {
                crate::run_protocol::OutputChannel::Assistant => "assistant",
                crate::run_protocol::OutputChannel::Status => "status",
            },
            text.clone(),
        ),
        RunEvent::ToolProposed {
            tool_name,
            mutation,
            ..
        } => (
            "tool_proposed",
            "tool",
            format!("{tool_name} mutation={mutation}"),
        ),
        RunEvent::PermissionRequested {
            tool_name,
            detail,
            risk_reason,
            ..
        } => (
            "permission_requested",
            "permission",
            join_search_text([
                Some(tool_name.as_str()),
                Some(detail.as_str()),
                risk_reason.as_deref(),
            ]),
        ),
        RunEvent::PermissionDecided { decision, .. } => {
            ("permission_decided", "permission", format!("{decision:?}"))
        }
        RunEvent::ToolStarted { tool_call_id } => ("tool_started", "tool", tool_call_id.clone()),
        RunEvent::ToolFinished {
            output_excerpt,
            outcome,
            ..
        } => (
            "tool_finished",
            "tool",
            match output_excerpt {
                Some(output) => format!("{outcome:?} {output}"),
                None => format!("{outcome:?}"),
            },
        ),
        RunEvent::ArtifactAdded {
            artifact_id,
            name,
            media_type,
            content_sha256,
            size_bytes,
            ..
        } => {
            let mut content = format!("{name} {media_type}");
            if let Some(text) = artifacts.and_then(|store| {
                searchable_artifact_text(
                    store,
                    artifact_id,
                    content_sha256,
                    media_type,
                    *size_bytes,
                )
            }) {
                content.push('\n');
                content.push_str(&text);
            }
            ("artifact_added", "artifact", content)
        }
        RunEvent::CheckpointLinked { label, .. } => {
            ("checkpoint_linked", "checkpoint", label.clone())
        }
        RunEvent::VerificationFinished {
            name,
            passed,
            summary,
            ..
        } => (
            "verification_finished",
            "verification",
            format!("{name} passed={passed} {summary}"),
        ),
        RunEvent::UsageRecorded { .. } => ("usage_recorded", "usage", String::new()),
        RunEvent::CancellationRequested { reason, .. } => (
            "cancellation_requested",
            "run",
            reason
                .clone()
                .unwrap_or_else(|| "cancellation requested".to_string()),
        ),
        RunEvent::ExternalMutationPrepared { summary, .. } => {
            ("external_mutation_prepared", "mutation", summary.clone())
        }
        RunEvent::ExternalMutationConfirmed { summary, .. } => {
            ("external_mutation_confirmed", "mutation", summary.clone())
        }
        RunEvent::AwaitingApproval { reason, .. } => (
            "awaiting_approval",
            "permission",
            reason
                .clone()
                .unwrap_or_else(|| "awaiting approval".to_string()),
        ),
        RunEvent::Paused { reason } => (
            "paused",
            "run",
            reason.clone().unwrap_or_else(|| "paused".to_string()),
        ),
        RunEvent::Cancelling { reason } => (
            "cancelling",
            "run",
            reason.clone().unwrap_or_else(|| "cancelling".to_string()),
        ),
        RunEvent::Completed { summary, .. } => (
            "completed",
            "run",
            summary.clone().unwrap_or_else(|| "completed".to_string()),
        ),
        RunEvent::Failed { code, message, .. } => ("failed", "run", format!("{code} {message}")),
        RunEvent::Cancelled { reason } => (
            "cancelled",
            "run",
            reason.clone().unwrap_or_else(|| "cancelled".to_string()),
        ),
        RunEvent::NeedsReconciliation {
            mutation_id,
            reason,
        } => (
            "needs_reconciliation",
            "mutation",
            format!("{mutation_id} {reason}"),
        ),
        // Indexed under "node" rather than "run": what someone searches for
        // after a migration is the machine, and the run id is already the
        // column this row is keyed by.
        RunEvent::MigrationDeparted {
            target_node_id,
            checkpoint_id,
            ..
        } => (
            "migration_departed",
            "node",
            format!("{target_node_id} {checkpoint_id}"),
        ),
        RunEvent::MigrationArrived {
            origin_node_id,
            origin_last_sequence,
            ..
        } => (
            "migration_arrived",
            "node",
            format!("{origin_node_id} {origin_last_sequence}"),
        ),
    }
}

fn searchable_artifact_text(
    store: &ArtifactStore,
    artifact_id: &str,
    expected_sha256: &str,
    media_type: &str,
    expected_size: u64,
) -> Option<String> {
    let normalized_media = media_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let textual = normalized_media.starts_with("text/")
        || matches!(
            normalized_media.as_str(),
            "application/json"
                | "application/ld+json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/javascript"
                | "application/sql"
        )
        || normalized_media.ends_with("+json")
        || normalized_media.ends_with("+xml");
    if !textual || expected_size == 0 || expected_size > MAX_SEARCH_ARTIFACT_BYTES {
        return None;
    }
    let bytes = store.read(artifact_id).ok()?;
    if u64::try_from(bytes.len()).ok()? != expected_size
        || sha256_hex(&bytes) != expected_sha256.to_ascii_lowercase()
    {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    Some(text)
}

fn join_search_text<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> String {
    values
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn validate_search_request(request: &GlobalSearchRequest) -> ProfileStoreResult<()> {
    if request.query.trim().is_empty() || request.query.len() > MAX_SEARCH_QUERY_BYTES {
        return Err(invalid(
            "search.query",
            format!("must contain 1..={MAX_SEARCH_QUERY_BYTES} non-whitespace bytes"),
        ));
    }
    if request.query.contains('\0') {
        return Err(invalid("search.query", "must not contain NUL"));
    }
    if request.limit == 0 || request.limit > MAX_SEARCH_LIMIT {
        return Err(invalid(
            "search.limit",
            format!("must be between 1 and {MAX_SEARCH_LIMIT}"),
        ));
    }
    if request
        .from_ms
        .zip(request.to_ms)
        .is_some_and(|(from, to)| from > to)
    {
        return Err(invalid("search.date", "from_ms must not exceed to_ms"));
    }
    for (path, value, max) in [
        (
            "search.modelKey",
            request.model_key.as_deref(),
            MAX_ID_BYTES,
        ),
        (
            "search.personaId",
            request.persona_id.as_deref(),
            MAX_ID_BYTES,
        ),
        (
            "search.workspacePath",
            request.workspace_path.as_deref(),
            MAX_PATH_BYTES,
        ),
    ] {
        if let Some(value) = value {
            validate_text(value, path, max)?;
        }
    }
    Ok(())
}

fn literal_fts_query(query: &str) -> ProfileStoreResult<String> {
    let terms = query
        .split_whitespace()
        .take(32)
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    if terms.is_empty() {
        return Err(invalid(
            "search.query",
            "does not contain a searchable term",
        ));
    }
    Ok(terms.join(" AND "))
}

fn ledger_has_fts5(ledger: &RunLedger) -> ProfileStoreResult<bool> {
    Ok(ledger.connection().query_row(
        "SELECT enabled FROM ledger_capabilities WHERE name = 'fts5'",
        [],
        |row| row.get::<_, i64>(0),
    )? == 1)
}

fn parse_source_kind(value: &str) -> ProfileStoreResult<SearchSourceKind> {
    match value {
        "message" => Ok(SearchSourceKind::Message),
        "actor_transcript" => Ok(SearchSourceKind::ActorTranscript),
        "run_event" => Ok(SearchSourceKind::RunEvent),
        other => Err(ProfileStoreError::Corrupt(format!(
            "unknown search source kind {other:?}"
        ))),
    }
}

fn sql_timestamp(value: u64) -> ProfileStoreResult<i64> {
    i64::try_from(value).map_err(|_| invalid("timestamp", "exceeds SQLite's signed range"))
}

fn sql_ordinal(value: usize, path: &str) -> ProfileStoreResult<i64> {
    i64::try_from(value).map_err(|_| invalid(path, "ordinal exceeds SQLite's signed range"))
}

fn stable_id(namespace: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(namespace.as_bytes());
    for part in parts {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    format!("{namespace}-{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        let mut output = value.chars().take(max_chars).collect::<String>();
        output.push('…');
        output
    }
}

fn now_ms() -> ProfileStoreResult<u64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("clock", "system time is before the Unix epoch"))?;
    u64::try_from(duration.as_millis()).map_err(|_| invalid("clock", "Unix timestamp exceeds u64"))
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> ProfileStoreError {
    ProfileStoreError::Invalid {
        path: path.into(),
        message: message.into(),
    }
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> ProfileStoreError {
    ProfileStoreError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::run_protocol::{
        ArtifactKind, CapabilityAssessment, CapabilityState, ClientIdentity, ClientKind,
        ModelCapabilitiesSnapshot, ModelTargetSnapshot, PermissionMode, PermissionPolicySnapshot,
        RunBudgets, RunKind, ToolOutcome, ToolPolicyDecision, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    struct TestEnv {
        root: PathBuf,
        database: PathBuf,
        source: PathBuf,
        artifact_root: PathBuf,
    }

    impl TestEnv {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let counter = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "little-monkey-profile-{label}-{}-{counter}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            Self {
                database: root.join("profile.sqlite3"),
                source: root.join("chat_sessions.json"),
                artifact_root: root.join("artifacts"),
                root,
            }
        }

        fn open(&self) -> (RunLedger, ArtifactStore) {
            (
                RunLedger::open(&self.database).unwrap(),
                ArtifactStore::new(&self.artifact_root).unwrap(),
            )
        }
    }

    impl Drop for TestEnv {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture_value() -> Value {
        json!({
            "futureRoot": {"kept": true},
            "activeSessionId": "session-active",
            "groups": [{
                "id": "group-main",
                "name": "Main",
                "kind": "folder",
                "futureGroup": "kept"
            }],
            "crews": [{
                "version": 1,
                "id": "crew-saved",
                "name": "Saved crew",
                "createdAt": 600,
                "updatedAt": 700,
                "futureCrew": {"kept": true}
            }],
            "sessions": [
                {
                    "id": "session-active",
                    "title": "Active session",
                    "createdAt": 1000,
                    "updatedAt": 2000,
                    "pinned": true,
                    "unread": false,
                    "archived": false,
                    "groupId": "group-main",
                    "modelTarget": {"kind": "ollama", "key": "model:active"},
                    "workspacePath": "/workspace/active",
                    "personaId": "persona-active",
                    "attachedStackIds": ["stack-a"],
                    "docChatMode": true,
                    "futureSession": {"kept": true},
                    "messages": [
                        {
                            "role": "user",
                            "content": [
                                {"type": "text", "text": "alpha needle active"},
                                {"type": "image_url", "image_url": {
                                    "url": "data:image/png;base64,aGVsbG8="
                                }}
                            ],
                            "futureMessage": "kept"
                        },
                        {
                            "role": "user",
                            "content": [
                                {"type": "text", "text": "second image"},
                                {"type": "image_url", "image_url": {
                                    "url": "data:image/png;base64,aGVsbG8="
                                }}
                            ]
                        },
                        {"role": "tool", "content": "tool output omega"}
                    ],
                    "subagentRuns": {
                        "task-1": [{"role": "assistant", "content": "subagent quartz"}]
                    },
                    "crewRun": {
                        "coordinator": {
                            "actorId": "actor-coordinator",
                            "modelTarget": {"key": "model:crew"},
                            "persona": {"id": "persona-crew"},
                            "transcript": [{
                                "id": "crew-entry-1",
                                "actorId": "actor-coordinator",
                                "at": 1500,
                                "kind": "tool_result",
                                "content": "crew quartz"
                            }]
                        },
                        "members": []
                    }
                },
                {
                    "id": "session-archived",
                    "title": "Archived session",
                    "createdAt": 3000,
                    "updatedAt": 4000,
                    "pinned": false,
                    "unread": true,
                    "archived": true,
                    "groupId": null,
                    "modelTarget": {"kind": "provider", "key": "model:archived"},
                    "workspacePath": "/workspace/archived",
                    "personaId": "persona-archived",
                    "messages": [{"role": "assistant", "content": "archived alpha needle"}],
                    "subagentRuns": {}
                }
            ]
        })
    }

    fn fixture_payload() -> String {
        serde_json::to_string(&fixture_value()).unwrap()
    }

    #[test]
    fn migration_is_idempotent_reopen_safe_and_extracts_inline_artifacts() {
        let env = TestEnv::new("migration");
        let payload = fixture_payload();
        let (mut ledger, artifacts) = env.open();
        assert_eq!(
            migration_status(&ledger, &env.source).unwrap().state,
            MigrationState::SourceMissing
        );
        fs::write(&env.source, payload.as_bytes()).unwrap();
        assert_eq!(
            migration_status(&ledger, &env.source).unwrap().state,
            MigrationState::Pending
        );

        let result = migrate_legacy_file(&mut ledger, &artifacts, &env.source).unwrap();
        assert_eq!(result.outcome, MigrationOutcome::Imported);
        assert_eq!(result.counts.sessions, 2);
        assert_eq!(result.counts.messages, 4);
        assert_eq!(result.counts.actor_transcripts, 2);
        assert_eq!(result.counts.attachment_occurrences, 2);
        assert_eq!(result.counts.unique_artifacts, 1);
        let recovery = result.recovery_path.clone().unwrap();
        assert_eq!(fs::read(&recovery).unwrap(), payload.as_bytes());

        let attachment = ledger
            .connection()
            .query_row(
                "SELECT content_sha256, byte_size FROM attachments",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .unwrap();
        assert_eq!(attachment.0, sha256_hex(b"hello"));
        assert_eq!(attachment.1, 5);
        assert_eq!(artifacts.read(&attachment.0).unwrap(), b"hello");
        assert_eq!(
            ledger
                .connection()
                .query_row(
                    "SELECT COUNT(*) FROM profile_message_attachment_links",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        let inline_count = ledger
            .connection()
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM messages
                      WHERE instr(content, ';base64,') > 0
                         OR instr(CAST(metadata_json AS TEXT), ';base64,') > 0)
                  + (SELECT COUNT(*) FROM sessions
                      WHERE instr(CAST(metadata_json AS TEXT), ';base64,') > 0)
                  + (SELECT COUNT(*) FROM profile_state
                      WHERE instr(CAST(root_metadata_json AS TEXT), ';base64,') > 0)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(inline_count, 0);

        let message_ids_before = ledger
            .connection()
            .prepare("SELECT message_id FROM messages ORDER BY session_id, ordinal")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let rerun = migrate_legacy_file(&mut ledger, &artifacts, &env.source).unwrap();
        assert_eq!(rerun.outcome, MigrationOutcome::NoChange);
        assert_eq!(
            migration_status(&ledger, &env.source).unwrap().state,
            MigrationState::Current
        );
        assert_eq!(rerun.recovery_path.as_deref(), Some(recovery.as_path()));
        let recovery_count = fs::read_dir(&env.root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".recovery-"))
            .count();
        assert_eq!(recovery_count, 1);
        fs::write(&env.source, format!("{payload}\n")).unwrap();
        assert_eq!(
            migration_status(&ledger, &env.source).unwrap().state,
            MigrationState::SourceChanged
        );
        drop(ledger);

        let reopened = RunLedger::open(&env.database).unwrap();
        let message_ids_after = reopened
            .connection()
            .prepare("SELECT message_id FROM messages ORDER BY session_id, ordinal")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(message_ids_before, message_ids_after);
        assert!(reopened.integrity_check().unwrap().is_ok());
    }

    #[test]
    fn corrupt_malformed_data_and_oversized_source_leave_live_database_untouched() {
        let env = TestEnv::new("invalid");
        let (mut ledger, artifacts) = env.open();
        fs::write(
            &env.source,
            br#"{"sessions":[],"activeSessionId":"missing"}"#,
        )
        .unwrap();
        assert!(migrate_legacy_file(&mut ledger, &artifacts, &env.source).is_err());
        assert_eq!(load_profile_counts(&ledger).unwrap().sessions, 0);
        assert!(load_stored_state(&ledger).unwrap().is_none());
        assert_eq!(
            fs::read_dir(&env.root)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains(".recovery-"))
                .count(),
            0
        );

        let malformed_data = fixture_payload().replace("aGVsbG8=", "%%%not-base64%%%");
        assert!(save_payload(&mut ledger, &artifacts, &malformed_data).is_err());
        assert_eq!(load_profile_counts(&ledger).unwrap().sessions, 0);

        let oversized = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&env.source)
            .unwrap();
        oversized.set_len(MAX_PROFILE_JSON_BYTES + 1).unwrap();
        drop(oversized);
        assert!(matches!(
            migrate_legacy_file(&mut ledger, &artifacts, &env.source),
            Err(ProfileStoreError::InputTooLarge { .. })
        ));
        assert_eq!(load_profile_counts(&ledger).unwrap().sessions, 0);
    }

    #[test]
    fn search_filters_active_archived_model_persona_workspace_and_removes_omitted_rows() {
        let env = TestEnv::new("search");
        let (mut ledger, artifacts) = env.open();
        let payload = fixture_payload();
        save_payload(&mut ledger, &artifacts, &payload).unwrap();

        let active = global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "alpha needle".to_string(),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id.as_deref(), Some("session-active"));
        assert!(active[0].snippet.len() <= MAX_SEARCH_SNIPPET_CHARS * 4);

        let archived = global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "alpha".to_string(),
                include_archived: true,
                model_key: Some("model:archived".to_string()),
                persona_id: Some("persona-archived".to_string()),
                workspace_path: Some("/workspace/archived".to_string()),
                from_ms: Some(3000),
                to_ms: Some(3000),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].session_id.as_deref(), Some("session-archived"));
        assert!(archived[0].archived);

        let tool = global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "omega".to_string(),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(tool.len(), 1);
        assert_eq!(tool[0].role, "tool");
        let transcripts = global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "quartz".to_string(),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(transcripts.len(), 2);
        assert!(transcripts
            .iter()
            .all(|hit| hit.source_kind == SearchSourceKind::ActorTranscript));

        let stable_id_before = ledger
            .connection()
            .query_row(
                "SELECT message_id FROM messages
                  WHERE session_id = 'session-active' AND ordinal = 0",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let mut updated = fixture_value();
        updated["sessions"].as_array_mut().unwrap().remove(1);
        updated["sessions"][0]["title"] = Value::String("Renamed active".to_string());
        save_payload(
            &mut ledger,
            &artifacts,
            &serde_json::to_string(&updated).unwrap(),
        )
        .unwrap();
        assert_eq!(load_profile_counts(&ledger).unwrap().sessions, 1);
        let stable_id_after = ledger
            .connection()
            .query_row(
                "SELECT message_id FROM messages
                  WHERE session_id = 'session-active' AND ordinal = 0",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        assert_eq!(stable_id_before, stable_id_after);
        assert!(!ledger
            .connection()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id = 'session-archived')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .unwrap());
        assert!(global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "archived alpha needle".to_string(),
                include_archived: true,
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn scoped_search_only_returns_native_allowlisted_workspaces_and_global_rows() {
        let env = TestEnv::new("scoped-search");
        let (mut ledger, artifacts) = env.open();
        save_payload(&mut ledger, &artifacts, &fixture_payload()).unwrap();
        let request = GlobalSearchRequest {
            query: "alpha needle".to_string(),
            include_archived: true,
            ..GlobalSearchRequest::default()
        };

        let active = global_search_with_artifacts_scoped(
            &mut ledger,
            &artifacts,
            &request,
            &["/workspace/active".to_string()],
        )
        .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id.as_deref(), Some("session-active"));

        let component_prefix_is_not_a_grant = global_search_with_artifacts_scoped(
            &mut ledger,
            &artifacts,
            &request,
            &["/workspace".to_string()],
        )
        .unwrap();
        assert!(component_prefix_is_not_a_grant.is_empty());

        let no_attached_roots =
            global_search_with_artifacts_scoped(&mut ledger, &artifacts, &request, &[]).unwrap();
        assert!(no_attached_roots.is_empty());

        let explicitly_detached = global_search_with_artifacts_scoped(
            &mut ledger,
            &artifacts,
            &GlobalSearchRequest {
                workspace_path: Some("/workspace/archived".to_string()),
                ..request.clone()
            },
            &["/workspace/active".to_string()],
        )
        .unwrap();
        assert!(explicitly_detached.is_empty());

        let global_env = TestEnv::new("scoped-search-global");
        let (mut global_ledger, global_artifacts) = global_env.open();
        let mut payload = fixture_value();
        payload["sessions"][0]["workspacePath"] = Value::Null;
        save_payload(
            &mut global_ledger,
            &global_artifacts,
            &serde_json::to_string(&payload).unwrap(),
        )
        .unwrap();
        let global_hits = global_search_with_artifacts_scoped(
            &mut global_ledger,
            &global_artifacts,
            &GlobalSearchRequest {
                query: "alpha needle".to_string(),
                ..GlobalSearchRequest::default()
            },
            &[],
        )
        .unwrap();
        assert_eq!(global_hits.len(), 1);
        assert!(global_hits[0].workspace_path.is_none());
    }

    #[test]
    fn run_tool_output_is_added_to_the_same_global_fts_index() {
        let env = TestEnv::new("run-search");
        let (mut ledger, artifacts) = env.open();
        save_payload(&mut ledger, &artifacts, &fixture_payload()).unwrap();
        let spec = run_spec("run-search-1");
        ledger.submit_run(&spec).unwrap();
        ledger
            .append_event(&run_envelope(
                "run-search-1",
                1,
                "run-event-1",
                RunEvent::Queued { queue: None },
            ))
            .unwrap();
        ledger
            .append_event(&run_envelope(
                "run-search-1",
                2,
                "run-event-2",
                RunEvent::ToolFinished {
                    tool_call_id: "tool-call-1".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    output_excerpt: Some("needle-from-shell unique-output".to_string()),
                    output_sha256: None,
                    duration_ms: 10,
                },
            ))
            .unwrap();

        let hits = global_search(
            &mut ledger,
            &GlobalSearchRequest {
                query: "unique-output".to_string(),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_kind, SearchSourceKind::RunEvent);
        assert_eq!(hits[0].run_id.as_deref(), Some("run-search-1"));
        assert_eq!(hits[0].role, "tool");
    }

    #[test]
    fn verified_text_artifact_bytes_are_searchable_when_store_is_available() {
        let env = TestEnv::new("artifact-search");
        let (mut ledger, artifacts) = env.open();
        save_payload(&mut ledger, &artifacts, &fixture_payload()).unwrap();
        let spec = run_spec("run-artifact-search-1");
        ledger.submit_run(&spec).unwrap();
        ledger
            .append_event(&run_envelope(
                "run-artifact-search-1",
                1,
                "artifact-search-event-1",
                RunEvent::Queued { queue: None },
            ))
            .unwrap();
        let blob = artifacts
            .put(b"verified artifact phrase banana-cobalt")
            .unwrap();
        ledger
            .append_event(&run_envelope(
                "run-artifact-search-1",
                2,
                "artifact-search-event-2",
                RunEvent::ArtifactAdded {
                    artifact_id: blob.id.clone(),
                    kind: ArtifactKind::Report,
                    name: "analysis.txt".to_string(),
                    media_type: "text/plain; charset=utf-8".to_string(),
                    content_sha256: blob.id,
                    size_bytes: blob.size,
                },
            ))
            .unwrap();

        let hits = global_search_with_artifacts(
            &mut ledger,
            &artifacts,
            &GlobalSearchRequest {
                query: "banana-cobalt".to_string(),
                ..GlobalSearchRequest::default()
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source_kind, SearchSourceKind::RunEvent);
        assert_eq!(hits[0].role, "artifact");
    }

    #[test]
    fn ten_thousand_message_fixture_reports_bounded_import_and_search_time() {
        let env = TestEnv::new("ten-thousand");
        let (mut ledger, artifacts) = env.open();
        let messages = (0..10_000)
            .map(|index| {
                json!({
                    "role": "user",
                    "content": format!("performance needle record {index}")
                })
            })
            .collect::<Vec<_>>();
        let payload = serde_json::to_string(&json!({
            "activeSessionId": "bulk-session",
            "groups": [],
            "sessions": [{
                "id": "bulk-session",
                "title": "Bulk",
                "messages": messages,
                "createdAt": 1000,
                "updatedAt": 2000,
                "pinned": false,
                "unread": false,
                "archived": false,
                "groupId": null,
                "workspacePath": null,
                "personaId": null,
                "subagentRuns": {}
            }]
        }))
        .unwrap();

        let import_started = Instant::now();
        let saved = save_payload(&mut ledger, &artifacts, &payload).unwrap();
        let import_elapsed = import_started.elapsed();
        assert_eq!(saved.counts.messages, 10_000);
        let request = GlobalSearchRequest {
            query: "performance needle".to_string(),
            limit: 10,
            ..GlobalSearchRequest::default()
        };
        let mut samples = Vec::with_capacity(50);
        for _ in 0..50 {
            let search_started = Instant::now();
            let hits = global_search(&mut ledger, &request).unwrap();
            samples.push(search_started.elapsed());
            assert_eq!(hits.len(), 10);
        }
        samples.sort_unstable();
        let p95 = samples[47];
        eprintln!("profile 10k timing: import={import_elapsed:?}, search_p95={p95:?}");
        assert!(
            import_elapsed.as_secs() < 30,
            "10k import took {import_elapsed:?}"
        );
        // Shared GitHub Actions runners have observably noisier tail latency
        // than a local dev machine (seen up to ~227ms against this 200ms
        // budget across unrelated PRs' CI runs) — widen the budget under CI
        // rather than chase a threshold no shared runner can hit reliably,
        // while keeping the tight local budget as the real regression signal.
        let budget_ms = if std::env::var_os("CI").is_some() {
            400
        } else {
            200
        };
        assert!(
            p95 < Duration::from_millis(budget_ms),
            "10k search p95 exceeded {budget_ms} ms: {p95:?}"
        );
    }

    fn test_client() -> ClientIdentity {
        ClientIdentity {
            client_id: "profile-test".to_string(),
            instance_id: "profile-instance".to_string(),
            kind: ClientKind::Test,
            version: "1.0.0-test".to_string(),
        }
    }

    fn capability() -> CapabilityAssessment {
        CapabilityAssessment {
            state: CapabilityState::Supported,
            evidence: "profile fixture".to_string(),
        }
    }

    fn capabilities() -> ModelCapabilitiesSnapshot {
        ModelCapabilitiesSnapshot {
            tool_calling: capability(),
            vision: capability(),
            embeddings: capability(),
            structured_output: capability(),
            image_generation: capability(),
            audio: capability(),
            runtime_lifecycle: capability(),
            fim: capability(),
            code_completion: capability(),
            inline_edit: capability(),
            fim_metadata: None,
        }
    }

    fn run_spec(run_id: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: format!("profile/{run_id}"),
            created_at_ms: 1_000,
            kind: RunKind::Background,
            submitted_by: test_client(),
            task: "searchable run task".to_string(),
            instructions: None,
            input_artifact_ids: Vec::new(),
            target: ModelTargetSnapshot::Ollama {
                target_id: "ollama-profile".to_string(),
                label: "Ollama profile".to_string(),
                base_url: "http://127.0.0.1:11434".to_string(),
                model: "qwen-test".to_string(),
                is_cloud: false,
                capabilities: capabilities(),
                estimated_memory_bytes: Some(1),
            },
            workspace: None,
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Manual,
                unattended: false,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: false,
                allow_external_mutations: false,
                egress_allowlist: None,
                channel_send: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 10,
                max_model_calls: 10,
                max_tool_calls: 10,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_000_000,
                max_event_count: 1_000,
            },
        }
    }

    fn run_envelope(
        run_id: &str,
        sequence: u64,
        event_id: &str,
        event: RunEvent,
    ) -> RunEventEnvelope {
        RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: event_id.to_string(),
            run_id: run_id.to_string(),
            sequence,
            occurred_at_ms: 2_000 + sequence,
            actor_id: None,
            emitter: test_client(),
            event,
        }
    }
}
