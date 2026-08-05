//! Persistent orchestration and Tauri commands for Knowledge Stacks 2.0.
//!
//! The active search generation is immutable. Refresh first enumerates and
//! validates every source, reuses prior chunk/vector rows only when both the
//! source hash and full pipeline fingerprint match, builds a staged SQLite
//! FTS/vector generation, and switches the active pointer last. Cancellation
//! or any connector/extractor/provider failure therefore leaves the previous
//! generation usable.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures_util::StreamExt;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::connectors;
use crate::knowledge_adapters::{
    media_type_for_path, source_object_from_bytes, HtmlPdfExtractor, OfficeOpenXmlExtractor,
    TesseractOcrProvider,
};
use crate::knowledge_pipeline::{
    run_ocr, ChunkingSpec, DocumentChunker, DocumentFormat, DocumentSecurityDeclaration,
    EmbeddingSpec as PipelineEmbeddingSpec, ExtractedDocument, ExtractionPolicy, ExtractorRegistry,
    GenerationBuild, GenerationDraft, GenerationStore, HybridSearchConfig, HybridSearchResponse,
    KnowledgeChunk, LocationAwareChunker, ObjectSnapshot, OcrAssetMetadata, OcrPageInput,
    PipelineError, PipelineLimits, RedactionPreview, RerankInput, RerankScore, Reranker,
    SensitiveDataMode, SensitiveDataScanner, SourceObject, UrlSourcePolicy,
    EMBEDDING_CONTRACT_VERSION, EXTRACTOR_CONTRACT_VERSION,
};
use crate::stacks::{EmbeddingBackend, KnowledgeStack};

const CATALOG_VERSION: u32 = 1;
const MAX_RETRY_HISTORY: usize = 20;
const MAX_HTTP_BYTES: usize = 32 * 1024 * 1024;
const MAX_URL_PAGES: usize = 200;
const MAX_URL_DEPTH: usize = 4;
const MAX_REDIRECTS: usize = 3;
const HTTP_TIMEOUT: Duration = Duration::from_secs(45);

/// The client both HTTP fetch paths share, pinned to the one address the
/// caller already validated.
///
/// [`HTTP_TIMEOUT`] is a **silence** budget here rather than a deadline for the
/// whole request. `reqwest::ClientBuilder::timeout` covers the body too, so
/// pairing it with the streaming reads in [`fetch_http`] and
/// [`fetch_connector_bytes`] gave a source 45 seconds to arrive in full —
/// against `max_file_bytes` capped at 32 MiB, that is 745 KB/s sustained, so a
/// large PDF over a slow or rate-limited host was aborted mid-download and
/// reported as a transport failure. `read_timeout` resets on every read: a source
/// that goes quiet for 45 seconds is still declared dead, one still making
/// progress is not. Size stays bounded where it already was, by the running total
/// in each loop against `MAX_HTTP_BYTES`.
///
/// `resolve` is what makes the request go to `socket` and nowhere else, so the
/// address the caller ran the SSRF guard against is the address actually dialled.
/// The redirect policy is `none` for the same reason, overriding the same-origin
/// rule [`crate::egress::hardened_with_read_budget`] supplies: a hop would be a
/// second URL this pinning never covered, and the pipeline follows redirects
/// itself (bounded by `MAX_REDIRECTS`) so each hop gets its own guarded lookup.
fn pinned_http_client(host: &str, socket: SocketAddr) -> Result<reqwest::Client, String> {
    crate::egress::hardened_with_read_budget(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, socket)
        .build()
        .map_err(|error| format!("Failed to create HTTP client: {error}"))
}
const REFRESH_LEASE_STALE_MS: u64 = 24 * 60 * 60 * 1_000;
const MAX_REFRESH_LEASE_BYTES: u64 = 4 * 1024;

static CATALOG_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static REFRESH_CANCELLATIONS: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();
static QUERY_CANCELLATIONS: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();

fn catalog_lock() -> &'static Mutex<()> {
    CATALOG_LOCK.get_or_init(|| Mutex::new(()))
}

fn cancellations() -> &'static Mutex<HashMap<String, CancellationToken>> {
    REFRESH_CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn query_cancellations() -> &'static Mutex<HashMap<String, CancellationToken>> {
    QUERY_CANCELLATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RefreshLeaseRecord {
    version: u32,
    stack_id: String,
    owner_pid: u32,
    acquired_at_ms: u64,
    token: String,
}

/// Filesystem lease shared by the desktop host and resident daemon. The
/// process-local cancellation registry prevents duplicate work inside one
/// host, while this create-new lease closes the cross-process race. A token
/// check on drop prevents an old owner from deleting a replacement lease.
#[derive(Debug)]
struct RefreshLease {
    path: PathBuf,
    directory: PathBuf,
    token: String,
}

impl Drop for RefreshLease {
    fn drop(&mut self) {
        let owns_current = fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RefreshLeaseRecord>(&bytes).ok())
            .is_some_and(|record| record.token == self.token);
        if owns_current {
            let _ = fs::remove_file(&self.path);
            let _ = sync_directory(&self.directory);
        }
    }
}

fn refresh_lease_directory(app_data: &Path) -> Result<PathBuf, String> {
    let directory = data_root_at(app_data)?.join("refresh-leases");
    fs::create_dir_all(&directory)
        .map_err(|error| format!("Failed to create {}: {error}", directory.display()))?;
    let metadata = fs::symlink_metadata(&directory)
        .map_err(|error| format!("Failed to inspect {}: {error}", directory.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Knowledge refresh lease directory is not a real directory".to_string());
    }
    Ok(directory)
}

fn read_refresh_lease(path: &Path) -> Result<RefreshLeaseRecord, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect refresh lease: {error}"))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_REFRESH_LEASE_BYTES
    {
        return Err("Knowledge refresh lease is unsafe or oversized".to_string());
    }
    let bytes = fs::read(path).map_err(|error| format!("Failed to read refresh lease: {error}"))?;
    let record: RefreshLeaseRecord = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Knowledge refresh lease is invalid: {error}"))?;
    if record.version != 1 {
        return Err("Knowledge refresh lease version is unsupported".to_string());
    }
    validate_id("refresh lease stack id", &record.stack_id)?;
    if record.token.is_empty() || record.token.len() > 128 || record.acquired_at_ms == 0 {
        return Err("Knowledge refresh lease fields are invalid".to_string());
    }
    Ok(record)
}

fn acquire_refresh_lease(app_data: &Path, stack_id: &str) -> Result<RefreshLease, String> {
    let directory = refresh_lease_directory(app_data)?;
    let path = directory.join(format!("{stack_id}.lock"));
    for _ in 0..4 {
        let token = Uuid::new_v4().simple().to_string();
        let record = RefreshLeaseRecord {
            version: 1,
            stack_id: stack_id.to_string(),
            owner_pid: std::process::id(),
            acquired_at_ms: now_ms().max(1),
            token: token.clone(),
        };
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let bytes = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
                if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
                    let _ = fs::remove_file(&path);
                    return Err(format!(
                        "Failed to publish Knowledge refresh lease: {error}"
                    ));
                }
                drop(file);
                sync_directory(&directory)?;
                return Ok(RefreshLease {
                    path,
                    directory,
                    token,
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = read_refresh_lease(&path)?;
                if now_ms().saturating_sub(existing.acquired_at_ms) <= REFRESH_LEASE_STALE_MS {
                    return Err(format!(
                        "A refresh for this stack is already running in process {}",
                        existing.owner_pid
                    ));
                }
                let stale =
                    directory.join(format!(".stale-{stack_id}-{}", Uuid::new_v4().simple()));
                match fs::rename(&path, &stale) {
                    Ok(()) => {
                        let _ = fs::remove_file(stale);
                        sync_directory(&directory)?;
                    }
                    Err(rename_error) if rename_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(rename_error) => {
                        return Err(format!(
                            "Failed to recover stale Knowledge refresh lease: {rename_error}"
                        ))
                    }
                }
            }
            Err(error) => {
                return Err(format!(
                    "Failed to acquire Knowledge refresh lease: {error}"
                ))
            }
        }
    }
    Err("Knowledge refresh lease changed repeatedly; retry later".to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn sha256(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn parse_http_modified_ms(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc2822(value)
        .ok()
        .and_then(|date| u64::try_from(date.timestamp_millis()).ok())
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!(
            "{label} must use only letters, digits, '-', '_', '.', or ':'"
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectorConfig {
    LocalFile {
        path: String,
    },
    LocalFolder {
        path: String,
    },
    Project {
        path: String,
    },
    Url {
        url: String,
        allowed_origin: String,
        max_depth: usize,
        max_pages: usize,
        obey_robots: bool,
        allow_loopback: bool,
    },
    Sitemap {
        url: String,
        allowed_origin: String,
        max_pages: usize,
        obey_robots: bool,
        allow_loopback: bool,
    },
    SelectedChats {
        session_ids: Vec<String>,
    },
    WebDav {
        url: String,
        username: String,
        credential_ref: String,
        allow_loopback: bool,
    },
    /// A GitHub repository, read via the already-authenticated `gh` CLI
    /// bridge (`m5_delivery::github`'s process boundary, through the
    /// generic `gh_api_json` helper) — no pasted token, ever.
    /// `connector_account_id` names a Connector Catalog GitHub account (see
    /// `connectors.rs`) purely for UI/identity purposes; the actual API
    /// calls always go through the single machine-wide `gh` session, same as
    /// `connectors_add_github`. Cursor = the last-seen commit SHA for
    /// `git_ref` (or the repo's default branch, if unset) — real incremental
    /// sync via `repos/{owner}/{repo}/compare/{cursor}...{ref}`, not just
    /// content-hash re-diffing.
    GitHubRepo {
        owner: String,
        repo: String,
        git_ref: Option<String>,
        path_prefix: Option<String>,
        connector_account_id: String,
    },
    /// An S3 (or S3-compatible: R2, MinIO, ...) bucket, listed and fetched
    /// with hand-rolled SigV4-signed requests (`connectors::sigv4_authorization`)
    /// — no AWS SDK dependency. `connector_account_id` supplies the access
    /// key/secret key (Connector Catalog `ConnectorProvider::S3`); the
    /// endpoint/bucket/region/prefix here are independent of what the
    /// account was originally verified against, so one credential can back
    /// several sources scoped to different prefixes or even buckets it can
    /// reach. Cursor = a per-object-key ETag map — only keys whose ETag
    /// changed are re-fetched.
    S3Bucket {
        endpoint: String,
        bucket: String,
        prefix: Option<String>,
        region: String,
        connector_account_id: String,
    },
    /// Exactly like `LocalFolder`, except a `notify`-backed filesystem
    /// watcher (`sync_watched_folder_watchers`) triggers an automatic
    /// debounced refresh on change, instead of requiring a manual refresh
    /// click.
    WatchedFolder {
        path: String,
        debounce_ms: u64,
    },
    /// A Notion workspace subtree, read via the Notion API's
    /// search/blocks-children endpoints with a catalog-stored integration
    /// token. `root_id` is the root page/database Notion id this source
    /// walks from. Cursor = the maximum `last_edited_time` observed across
    /// visited pages — real incremental sync via Notion's own search
    /// `sort.timestamp = "last_edited_time"`, not just content-hash
    /// re-diffing.
    NotionPages {
        connector_account_id: String,
        root_id: String,
    },
    /// A fixed set of Slack channels, read via `conversations.history` with
    /// a catalog-stored bot token. Cursor = a per-channel map of Slack's own
    /// `oldest`-style message timestamp cursor.
    SlackChannels {
        connector_account_id: String,
        channel_ids: Vec<String>,
    },
    /// A Jira project, read via the REST `/search` (JQL) endpoint with a
    /// catalog-stored API token + account email. Cursor = a JQL
    /// `updated >= <cursor>` bound (the maximum `updated` timestamp
    /// observed).
    JiraProject {
        connector_account_id: String,
        project_key: String,
    },
}

// --- Non-goals (see the ROADMAP "External Knowledge Sync Pipelines" entry
// and this build's own Non-Goals convention) ---------------------------------
//
// SharePoint and Google Drive connectors are NOT implemented here, and never
// will be through this token-based catalog: both only expose their
// documents through Microsoft Graph / Google Workspace APIs that require a
// **registered OAuth application** (a real client id/secret and an approved
// redirect URI with the respective platform), which is explicitly out of
// scope for this build (see this build's constraints). There is no
// token/PAT-based fallback that reaches a user's actual SharePoint/Drive
// document set, so faking either with, say, a WebDAV-style endpoint would
// misrepresent what's actually connected. GitLab is a stretch, same-shaped
// as `GitHubRepo` (a `glab`-CLI or PAT bridge) — not implemented in this
// pass either; unlike SharePoint/Drive it needs no OAuth app and could be
// added later without an architecture change, so it is a plain scheduling
// non-goal rather than an OAuth-blocked one.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectorObjectState {
    pub object_id: String,
    pub canonical_uri: String,
    pub content_sha256: String,
    pub etag: Option<String>,
    pub modified_unix_ms: Option<u64>,
    pub chunk_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConnectorRetry {
    pub attempted_at_ms: u64,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeSource {
    pub id: String,
    pub stack_id: String,
    pub label: String,
    pub enabled: bool,
    pub connector: ConnectorConfig,
    pub cursor: Option<String>,
    pub checkpoint: Option<String>,
    pub last_refresh_at_ms: Option<u64>,
    pub last_error: Option<String>,
    pub objects: Vec<ConnectorObjectState>,
    pub retries: Vec<ConnectorRetry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeBackgroundRefreshConfig {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub stack_ids: Vec<String>,
    pub last_attempt_ms: Option<u64>,
    pub last_success_ms: Option<u64>,
    pub next_due_ms: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl Default for KnowledgeBackgroundRefreshConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 60,
            stack_ids: Vec::new(),
            last_attempt_ms: None,
            last_success_ms: None,
            next_due_ms: None,
            last_error: None,
            consecutive_failures: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SaveKnowledgeBackgroundRefreshConfig {
    pub enabled: bool,
    pub interval_minutes: u64,
    pub stack_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KnowledgeBackgroundRefreshOutcome {
    pub due: bool,
    pub refreshed_stack_ids: Vec<String>,
    pub failures: Vec<String>,
    pub next_due_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct KnowledgeCatalog {
    version: u32,
    sources: Vec<KnowledgeSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeOcrConfig {
    pub enabled: bool,
    pub executable_path: Option<String>,
    pub pdf_renderer_path: Option<String>,
    pub asset: Option<OcrAssetMetadata>,
    pub languages: Vec<String>,
    pub low_confidence_micros: u32,
}

impl Default for KnowledgeOcrConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            executable_path: None,
            pdf_renderer_path: None,
            asset: None,
            languages: vec!["eng".to_string()],
            low_confidence_micros: 800_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrInstallRequest {
    pub url: String,
    pub version: String,
    pub expected_sha256: String,
    pub size_bytes: u64,
    pub license_name: String,
    pub license_url: Option<String>,
    pub provenance: String,
    pub languages: Vec<String>,
}

impl Default for KnowledgeCatalog {
    fn default() -> Self {
        Self {
            version: CATALOG_VERSION,
            sources: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeRefreshProgress {
    pub stack_id: String,
    pub source_id: Option<String>,
    pub phase: String,
    pub objects_done: usize,
    pub objects_total: usize,
    pub chunks: usize,
    pub reused_chunks: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeRefreshReport {
    pub stack_id: String,
    pub generation_id: String,
    pub parent_generation_id: Option<String>,
    pub source_count: usize,
    pub object_count: usize,
    pub changed_objects: usize,
    pub unchanged_objects: usize,
    pub deleted_objects: usize,
    pub embedded_chunks: usize,
    pub reused_chunks: usize,
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeQueryRequest {
    pub stack_id: String,
    #[serde(default)]
    pub query_id: Option<String>,
    pub query: String,
    #[serde(default)]
    pub config: HybridSearchConfig,
    #[serde(default)]
    pub excluded_source_ids: Vec<String>,
    #[serde(default)]
    pub rerank: bool,
    #[serde(default = "default_token_budget")]
    pub token_budget: usize,
}

fn default_token_budget() -> usize {
    4_096
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeInspectorResponse {
    pub query_id: String,
    pub normalized_query: String,
    pub excluded_source_ids: Vec<String>,
    pub token_budget: usize,
    pub estimated_context_tokens: usize,
    pub final_context: String,
    pub search: HybridSearchResponse,
}

fn data_root(app: &AppHandle) -> Result<PathBuf, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    data_root_at(&app_data)
}

fn data_root_at(app_data: &Path) -> Result<PathBuf, String> {
    let root = app_data.join("knowledge-v2");
    fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create {}: {error}", root.display()))?;
    Ok(root)
}

fn catalog_path(root: &Path) -> PathBuf {
    root.join("catalog.json")
}

fn ocr_config_path(root: &Path) -> PathBuf {
    root.join("ocr.json")
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let directory = fs::File::open(path)
            .map_err(|error| format!("Failed to open {} for sync: {error}", path.display()))?;
        directory
            .sync_all()
            .map_err(|error| format!("Failed to sync {}: {error}", path.display()))?;
    }
    Ok(())
}

/// Publish a private state file durably on both Unix and Windows.
///
/// Unix `rename` replaces an existing regular file atomically. Windows does
/// not guarantee that behavior for `std::fs::rename`, so the fallback first
/// moves the old file aside and restores it if publishing the new file fails.
fn atomic_write_private(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} path has no parent"))?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(format!("{label} target is not a regular file"));
        }
    }
    let temporary = parent.join(format!(".knowledge-{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .map_err(|error| format!("Failed to create {label}: {error}"))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("Failed to write {label}: {error}"));
    }
    drop(file);

    match fs::rename(&temporary, path) {
        Ok(()) => sync_directory(parent),
        Err(first_error) if path.exists() => {
            let backup = parent.join(format!(".knowledge-{}.bak", Uuid::new_v4().simple()));
            fs::rename(path, &backup).map_err(|error| {
                let _ = fs::remove_file(&temporary);
                format!("Failed to prepare {label} replacement after {first_error}: {error}")
            })?;
            if let Err(error) = fs::rename(&temporary, path) {
                let restore_error = fs::rename(&backup, path).err();
                let _ = fs::remove_file(&temporary);
                return Err(match restore_error {
                    Some(restore) => format!(
                        "Failed to publish {label}: {error}; restoring the previous file also failed: {restore}"
                    ),
                    None => format!("Failed to publish {label}: {error}"),
                });
            }
            fs::remove_file(&backup)
                .map_err(|error| format!("Failed to remove the old {label}: {error}"))?;
            sync_directory(parent)
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(format!("Failed to publish {label}: {error}"))
        }
    }
}

fn background_refresh_path(root: &Path) -> PathBuf {
    root.join("background-refresh.json")
}

fn validate_background_refresh_config(
    mut config: KnowledgeBackgroundRefreshConfig,
) -> Result<KnowledgeBackgroundRefreshConfig, String> {
    if !(5..=7 * 24 * 60).contains(&config.interval_minutes) {
        return Err(
            "Knowledge background interval must be between 5 minutes and 7 days".to_string(),
        );
    }
    if config.stack_ids.len() > 128 {
        return Err("Knowledge background refresh cannot name more than 128 stacks".to_string());
    }
    if config.consecutive_failures > 1_000_000 {
        return Err("Knowledge background failure counter is invalid".to_string());
    }
    if config
        .last_error
        .as_ref()
        .is_some_and(|error| error.len() > 8 * 1024 || error.contains('\0'))
    {
        return Err("Knowledge background error detail is invalid".to_string());
    }
    config.stack_ids.sort();
    config.stack_ids.dedup();
    for stack_id in &config.stack_ids {
        validate_id("stack id", stack_id)?;
    }
    if !config.enabled {
        config.next_due_ms = None;
    }
    Ok(config)
}

pub fn load_background_refresh_config_at(
    app_data: &Path,
) -> Result<KnowledgeBackgroundRefreshConfig, String> {
    let root = data_root_at(app_data)?;
    let path = background_refresh_path(&root);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(KnowledgeBackgroundRefreshConfig::default())
        }
        Err(error) => Err(format!("Failed to inspect {}: {error}", path.display())),
        Ok(metadata) if !metadata.file_type().is_file() || metadata.len() > 64 * 1024 => {
            Err("Knowledge background configuration is not a bounded regular file".to_string())
        }
        Ok(_) => {
            let bytes = fs::read(&path)
                .map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
            let config = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Invalid Knowledge background configuration: {error}"))?;
            validate_background_refresh_config(config)
        }
    }
}

fn save_background_refresh_config_at(
    app_data: &Path,
    config: &KnowledgeBackgroundRefreshConfig,
) -> Result<(), String> {
    let root = data_root_at(app_data)?;
    let config = validate_background_refresh_config(config.clone())?;
    let bytes = serde_json::to_vec_pretty(&config).map_err(|error| error.to_string())?;
    atomic_write_private(
        &background_refresh_path(&root),
        &bytes,
        "Knowledge background configuration",
    )
}

#[tauri::command]
pub fn knowledge_v2_background_config_get(
    app: AppHandle,
) -> Result<KnowledgeBackgroundRefreshConfig, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    load_background_refresh_config_at(&app_data)
}

#[tauri::command]
pub fn knowledge_v2_background_config_save(
    app: AppHandle,
    request: SaveKnowledgeBackgroundRefreshConfig,
) -> Result<KnowledgeBackgroundRefreshConfig, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    let previous = load_background_refresh_config_at(&app_data).unwrap_or_default();
    let config = validate_background_refresh_config(KnowledgeBackgroundRefreshConfig {
        enabled: request.enabled,
        interval_minutes: request.interval_minutes,
        stack_ids: request.stack_ids,
        last_attempt_ms: previous.last_attempt_ms,
        last_success_ms: previous.last_success_ms,
        last_error: previous.last_error,
        consecutive_failures: previous.consecutive_failures,
        next_due_ms: request
            .enabled
            .then_some(now_ms().saturating_add(request.interval_minutes.saturating_mul(60_000))),
    })?;
    save_background_refresh_config_at(&app_data, &config)?;
    Ok(config)
}

fn load_ocr_config(root: &Path) -> Result<KnowledgeOcrConfig, String> {
    let path = ocr_config_path(root);
    if !path.exists() {
        return Ok(KnowledgeOcrConfig::default());
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if bytes.len() > 256 * 1024 {
        return Err("OCR configuration exceeds 256 KiB".to_string());
    }
    let config: KnowledgeOcrConfig = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse OCR configuration: {error}"))?;
    validate_ocr_config(&config)?;
    Ok(config)
}

fn save_ocr_config(root: &Path, config: &KnowledgeOcrConfig) -> Result<(), String> {
    validate_ocr_config(config)?;
    let path = ocr_config_path(root);
    let bytes = serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?;
    atomic_write_private(&path, &bytes, "OCR configuration")
}

fn validate_regular_executable(path: &str, label: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(format!("{label} path must be absolute and unambiguous"));
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {label} {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} must be a regular non-symlink file"));
    }
    fs::canonicalize(path).map_err(|error| format!("Cannot resolve {label}: {error}"))
}

fn validate_ocr_config(config: &KnowledgeOcrConfig) -> Result<(), String> {
    if config.languages.is_empty()
        || config.languages.len() > 16
        || config.languages.iter().any(|language| {
            language.is_empty()
                || language.len() > 32
                || !language
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        || config.low_confidence_micros > 1_000_000
    {
        return Err("Invalid OCR language or confidence configuration".to_string());
    }
    if config.enabled {
        let executable = config
            .executable_path
            .as_deref()
            .ok_or_else(|| "Enabled OCR has no executable".to_string())?;
        validate_regular_executable(executable, "OCR executable")?;
        config
            .asset
            .as_ref()
            .ok_or_else(|| "Enabled OCR has no asset metadata".to_string())?
            .validate()
            .map_err(|error| error.to_string())?;
    }
    if let Some(renderer) = &config.pdf_renderer_path {
        validate_regular_executable(renderer, "PDF renderer")?;
    }
    Ok(())
}

fn load_catalog(root: &Path) -> Result<KnowledgeCatalog, String> {
    let path = catalog_path(root);
    if !path.exists() {
        return Ok(KnowledgeCatalog::default());
    }
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read {}: {error}", path.display()))?;
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("Knowledge connector catalog exceeds 8 MiB".to_string());
    }
    let catalog: KnowledgeCatalog = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse {}: {error}", path.display()))?;
    if catalog.version != CATALOG_VERSION {
        return Err(format!(
            "Unsupported Knowledge connector catalog version {}",
            catalog.version
        ));
    }
    let mut ids = BTreeSet::new();
    for source in &catalog.sources {
        validate_id("source id", &source.id)?;
        validate_id("stack id", &source.stack_id)?;
        if !ids.insert(&source.id) {
            return Err(format!("Duplicate Knowledge source id: {}", source.id));
        }
    }
    Ok(catalog)
}

fn save_catalog(root: &Path, catalog: &KnowledgeCatalog) -> Result<(), String> {
    let path = catalog_path(root);
    let bytes = serde_json::to_vec_pretty(catalog)
        .map_err(|error| format!("Failed to serialize connector catalog: {error}"))?;
    atomic_write_private(&path, &bytes, "Knowledge connector catalog")
}

fn validate_connector(connector: &ConnectorConfig) -> Result<(), String> {
    match connector {
        ConnectorConfig::LocalFile { path }
        | ConnectorConfig::LocalFolder { path }
        | ConnectorConfig::Project { path } => {
            let path = Path::new(path);
            if !path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err("Local Knowledge source path must be absolute and unambiguous".into());
            }
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() {
                return Err("Symlink Knowledge sources are disabled".to_string());
            }
            match connector {
                ConnectorConfig::LocalFile { .. } if !metadata.is_file() => {
                    return Err("Selected local source is not a file".to_string())
                }
                ConnectorConfig::LocalFolder { .. } | ConnectorConfig::Project { .. }
                    if !metadata.is_dir() =>
                {
                    return Err("Selected local source is not a directory".to_string())
                }
                _ => {}
            }
        }
        ConnectorConfig::Url {
            url,
            allowed_origin,
            max_depth,
            max_pages,
            ..
        } => {
            validate_url_config(url, allowed_origin)?;
            if *max_depth > MAX_URL_DEPTH || *max_pages == 0 || *max_pages > MAX_URL_PAGES {
                return Err("URL crawl depth/page count is outside allowed limits".to_string());
            }
        }
        ConnectorConfig::Sitemap {
            url,
            allowed_origin,
            max_pages,
            ..
        } => {
            validate_url_config(url, allowed_origin)?;
            if *max_pages == 0 || *max_pages > MAX_URL_PAGES {
                return Err("Sitemap page count is outside allowed limits".to_string());
            }
        }
        ConnectorConfig::SelectedChats { session_ids } => {
            if session_ids.is_empty() || session_ids.len() > 200 {
                return Err("Select between one and 200 conversations".to_string());
            }
            for id in session_ids {
                validate_id("session id", id)?;
            }
        }
        ConnectorConfig::WebDav {
            url,
            credential_ref,
            ..
        } => {
            let parsed = Url::parse(url).map_err(|error| format!("Invalid WebDAV URL: {error}"))?;
            if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
                return Err("WebDAV requires an absolute HTTP(S) URL".to_string());
            }
            validate_id("credential ref", credential_ref)?;
        }
        ConnectorConfig::GitHubRepo {
            owner,
            repo,
            git_ref,
            path_prefix,
            connector_account_id,
        } => {
            validate_repo_segment("GitHub owner", owner)?;
            validate_repo_segment("GitHub repo", repo)?;
            if let Some(git_ref) = git_ref {
                validate_git_ref(git_ref)?;
            }
            if let Some(prefix) = path_prefix {
                validate_relative_prefix("GitHub path prefix", prefix)?;
            }
            let account = connectors::account_by_id(connector_account_id)?;
            if account.provider != connectors::ConnectorProvider::Github {
                return Err("The selected connector account is not a GitHub account".to_string());
            }
        }
        ConnectorConfig::S3Bucket {
            endpoint,
            bucket,
            prefix,
            region,
            connector_account_id,
        } => {
            let parsed =
                Url::parse(endpoint).map_err(|error| format!("Invalid S3 endpoint: {error}"))?;
            if !matches!(parsed.scheme(), "https" | "http") || parsed.host_str().is_none() {
                return Err("S3 endpoint must be an absolute HTTP(S) URL".to_string());
            }
            validate_s3_bucket_name(bucket)?;
            validate_s3_region_name(region)?;
            if let Some(prefix) = prefix {
                validate_relative_prefix("S3 prefix", prefix)?;
            }
            let account = connectors::account_by_id(connector_account_id)?;
            if account.provider != connectors::ConnectorProvider::S3 {
                return Err("The selected connector account is not an S3 account".to_string());
            }
        }
        ConnectorConfig::WatchedFolder { path, debounce_ms } => {
            let path = Path::new(path);
            if !path.is_absolute()
                || path.components().any(|component| {
                    matches!(
                        component,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err("Watched folder path must be absolute and unambiguous".to_string());
            }
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err("Watched folder must be a real directory, not a symlink".to_string());
            }
            if !(200..=600_000).contains(debounce_ms) {
                return Err("Watched folder debounce must be between 200ms and 10 minutes".to_string());
            }
        }
        ConnectorConfig::NotionPages {
            connector_account_id,
            root_id,
        } => {
            validate_notion_id(root_id)?;
            let account = connectors::account_by_id(connector_account_id)?;
            if account.provider != connectors::ConnectorProvider::Notion {
                return Err("The selected connector account is not a Notion account".to_string());
            }
        }
        ConnectorConfig::SlackChannels {
            connector_account_id,
            channel_ids,
        } => {
            if channel_ids.is_empty() || channel_ids.len() > 50 {
                return Err("Select between one and 50 Slack channels".to_string());
            }
            for channel_id in channel_ids {
                validate_slack_channel_id(channel_id)?;
            }
            let account = connectors::account_by_id(connector_account_id)?;
            if account.provider != connectors::ConnectorProvider::Slack {
                return Err("The selected connector account is not a Slack account".to_string());
            }
        }
        ConnectorConfig::JiraProject {
            connector_account_id,
            project_key,
        } => {
            validate_jira_project_key(project_key)?;
            let account = connectors::account_by_id(connector_account_id)?;
            if account.provider != connectors::ConnectorProvider::Jira {
                return Err("The selected connector account is not a Jira account".to_string());
            }
        }
    }
    Ok(())
}

fn validate_repo_segment(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(format!(
            "{label} must use only letters, digits, '-', '_', or '.'"
        ));
    }
    Ok(())
}

fn validate_git_ref(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 250
        || value.starts_with('/')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'/'))
    {
        return Err("Invalid git ref".to_string());
    }
    Ok(())
}

fn validate_relative_prefix(label: &str, value: &str) -> Result<(), String> {
    if value.len() > 500
        || value.starts_with('/')
        || value.contains("..")
        || value.contains('\0')
    {
        return Err(format!("{label} must be a relative path with no '..' segments"));
    }
    Ok(())
}

fn validate_s3_bucket_name(bucket: &str) -> Result<(), String> {
    let valid = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err("Invalid S3 bucket name".to_string())
    }
}

fn validate_s3_region_name(region: &str) -> Result<(), String> {
    let valid = !region.is_empty()
        && region.len() <= 40
        && region.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err("Invalid S3 region".to_string())
    }
}

fn validate_notion_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 100
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid Notion page/database id".to_string());
    }
    Ok(())
}

fn validate_slack_channel_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 32 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("Invalid Slack channel id".to_string());
    }
    Ok(())
}

fn validate_jira_project_key(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 40
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("Invalid Jira project key".to_string());
    }
    Ok(())
}

fn validate_url_config(url: &str, allowed_origin: &str) -> Result<(), String> {
    let parsed = Url::parse(url).map_err(|error| format!("Invalid source URL: {error}"))?;
    let origin = origin_of(&parsed)?;
    let normalized_allowed = origin_of(
        &Url::parse(allowed_origin).map_err(|error| format!("Invalid allowed origin: {error}"))?,
    )?;
    if origin != normalized_allowed {
        return Err("Source URL must belong to its explicit allowed origin".to_string());
    }
    Ok(())
}

fn origin_of(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let default =
        (url.scheme() == "https" && port == 443) || (url.scheme() == "http" && port == 80);
    Ok(if default {
        format!("{}://{}", url.scheme(), host.to_ascii_lowercase())
    } else {
        format!("{}://{}:{port}", url.scheme(), host.to_ascii_lowercase())
    })
}

#[tauri::command]
pub fn knowledge_v2_list_sources(
    app: AppHandle,
    stack_id: Option<String>,
) -> Result<Vec<KnowledgeSource>, String> {
    let root = data_root(&app)?;
    let _guard = catalog_lock()
        .lock()
        .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
    let mut sources = load_catalog(&root)?.sources;
    if let Some(stack_id) = stack_id {
        validate_id("stack id", &stack_id)?;
        sources.retain(|source| source.stack_id == stack_id);
    }
    sources.sort_by(|left, right| {
        left.stack_id
            .cmp(&right.stack_id)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(sources)
}

#[tauri::command]
pub fn knowledge_v2_add_source(
    app: AppHandle,
    stack_id: String,
    label: String,
    connector: ConnectorConfig,
    webdav_password: Option<String>,
) -> Result<KnowledgeSource, String> {
    validate_id("stack id", &stack_id)?;
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 120 {
        return Err("Source label must contain 1-120 characters".to_string());
    }
    validate_connector(&connector)?;
    if let ConnectorConfig::WebDav { credential_ref, .. } = &connector {
        if let Some(password) = webdav_password {
            if password.len() > 16 * 1024 {
                return Err("WebDAV credential is too large".to_string());
            }
            keyring::Entry::new("little-monkey-knowledge-webdav", credential_ref)
                .map_err(|error| format!("Failed to access keychain: {error}"))?
                .set_password(&password)
                .map_err(|error| format!("Failed to save WebDAV credential: {error}"))?;
        }
    }
    let root = data_root(&app)?;
    let _guard = catalog_lock()
        .lock()
        .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
    let mut catalog = load_catalog(&root)?;
    let source = KnowledgeSource {
        id: Uuid::new_v4().to_string(),
        stack_id,
        label: label.to_string(),
        enabled: true,
        connector,
        cursor: None,
        checkpoint: None,
        last_refresh_at_ms: None,
        last_error: None,
        objects: Vec::new(),
        retries: Vec::new(),
    };
    catalog.sources.push(source.clone());
    save_catalog(&root, &catalog)?;
    sync_watched_folder_watchers(&app);
    Ok(source)
}

#[tauri::command]
pub fn knowledge_v2_update_source(
    app: AppHandle,
    source_id: String,
    label: String,
    enabled: bool,
    connector: ConnectorConfig,
    webdav_password: Option<String>,
) -> Result<KnowledgeSource, String> {
    validate_id("source id", &source_id)?;
    validate_connector(&connector)?;
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 120 {
        return Err("Source label must contain 1-120 characters".to_string());
    }
    if let ConnectorConfig::WebDav { credential_ref, .. } = &connector {
        if let Some(password) = webdav_password {
            keyring::Entry::new("little-monkey-knowledge-webdav", credential_ref)
                .map_err(|error| format!("Failed to access keychain: {error}"))?
                .set_password(&password)
                .map_err(|error| format!("Failed to save WebDAV credential: {error}"))?;
        }
    }
    let root = data_root(&app)?;
    let _guard = catalog_lock()
        .lock()
        .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
    let mut catalog = load_catalog(&root)?;
    let source = catalog
        .sources
        .iter_mut()
        .find(|source| source.id == source_id)
        .ok_or_else(|| "Knowledge source not found".to_string())?;
    source.label = label.to_string();
    source.enabled = enabled;
    if source.connector != connector {
        source.connector = connector;
        source.cursor = None;
        source.checkpoint = None;
        source.last_error = None;
        let _ = fs::remove_dir_all(root.join("connector-cache").join(&source_id));
    }
    let result = source.clone();
    save_catalog(&root, &catalog)?;
    sync_watched_folder_watchers(&app);
    Ok(result)
}

#[tauri::command]
pub fn knowledge_v2_remove_source(app: AppHandle, source_id: String) -> Result<(), String> {
    validate_id("source id", &source_id)?;
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    let root = data_root_at(&app_data)?;
    let _guard = catalog_lock()
        .lock()
        .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
    let mut catalog = load_catalog(&root)?;
    let original = catalog.sources.len();
    let removed = catalog
        .sources
        .iter()
        .find(|source| source.id == source_id)
        .cloned();
    catalog.sources.retain(|source| source.id != source_id);
    if catalog.sources.len() == original {
        return Err("Knowledge source not found".to_string());
    }
    let removed = removed.expect("the retained length changed only after finding the source");
    // A source removal is a privacy boundary, not merely catalog metadata.
    // Publish a new immutable generation without any of its chunks before
    // making the catalog deletion visible. A concurrent refresh owns the same
    // cross-process lease and therefore makes this operation fail closed.
    let _lease = acquire_refresh_lease(&app_data, &removed.stack_id)?;
    remove_source_generation(&app_data, &root, &removed.stack_id, &source_id)?;
    save_catalog(&root, &catalog)?;
    let _ = fs::remove_dir_all(root.join("connector-cache").join(&source_id));
    sync_watched_folder_watchers(&app);
    if let KnowledgeSource {
        connector: ConnectorConfig::WebDav { credential_ref, .. },
        ..
    } = removed
    {
        if !catalog.sources.iter().any(|source| {
            matches!(&source.connector, ConnectorConfig::WebDav { credential_ref: other, .. } if other == &credential_ref)
        }) {
            if let Ok(entry) = keyring::Entry::new("little-monkey-knowledge-webdav", &credential_ref)
            {
                let _ = entry.delete_credential();
            }
        }
    }
    Ok(())
}

fn remove_source_generation(
    app_data: &Path,
    root: &Path,
    stack_id: &str,
    source_id: &str,
) -> Result<(), String> {
    let store = GenerationStore::new(root.join("indexes")).map_err(|error| error.to_string())?;
    let Some(active) = store.active(stack_id).map_err(|error| error.to_string())? else {
        return Ok(());
    };
    let kept_objects = active
        .manifest
        .objects
        .iter()
        .filter(|object| object.source_id != source_id)
        .cloned()
        .collect::<Vec<_>>();
    if kept_objects.len() == active.manifest.objects.len() {
        return Ok(());
    }
    let kept_chunk_ids = kept_objects
        .iter()
        .flat_map(|object| object.chunk_ids.iter().cloned())
        .collect::<BTreeSet<_>>();
    let entries = store
        .open_active_index(stack_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Active Knowledge generation lost its index".to_string())?
        .entries()
        .map_err(|error| error.to_string())?;
    let (chunks, vectors): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .filter(|(chunk, _)| kept_chunk_ids.contains(&chunk.chunk_id))
        .unzip();
    if chunks.len() != kept_chunk_ids.len() {
        return Err(
            "Active Knowledge generation is missing chunks required by retained sources"
                .to_string(),
        );
    }
    let generation_id = Uuid::new_v4().to_string();
    let build = GenerationBuild {
        draft: GenerationDraft {
            stack_id: stack_id.to_string(),
            generation_id: generation_id.clone(),
            parent_generation_id: Some(active.manifest.generation_id),
            created_unix_ms: now_ms(),
            pipeline_fingerprint: active.manifest.pipeline_fingerprint,
            embedding_spec: active.manifest.embedding_spec,
            objects: kept_objects,
        },
        chunks,
        vectors,
    };
    let cancel = CancellationToken::new();
    let staged = store
        .stage(&build, &PipelineLimits::default(), &cancel)
        .map_err(|error| error.to_string())?;
    store
        .activate(staged, &cancel)
        .map_err(|error| error.to_string())?;
    crate::stacks::mark_v2_indexed_impl(
        &app_data.join("stacks"),
        stack_id,
        now_ms(),
        build.chunks.len(),
    )?;
    Ok(())
}

#[tauri::command]
pub fn knowledge_v2_cancel_refresh(stack_id: String) -> Result<bool, String> {
    validate_id("stack id", &stack_id)?;
    let token = cancellations()
        .lock()
        .map_err(|_| "Knowledge cancellation lock poisoned".to_string())?
        .get(&stack_id)
        .cloned();
    if let Some(token) = token {
        token.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

struct CancellationRegistration {
    stack_id: String,
}

impl Drop for CancellationRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = cancellations().lock() {
            map.remove(&self.stack_id);
        }
    }
}

fn register_cancellation(
    stack_id: &str,
) -> Result<(CancellationToken, CancellationRegistration), String> {
    let token = CancellationToken::new();
    let mut map = cancellations()
        .lock()
        .map_err(|_| "Knowledge cancellation lock poisoned".to_string())?;
    match map.entry(stack_id.to_string()) {
        std::collections::hash_map::Entry::Occupied(_) => {
            return Err("A refresh for this stack is already running".to_string())
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(token.clone());
        }
    }
    Ok((
        token,
        CancellationRegistration {
            stack_id: stack_id.to_string(),
        },
    ))
}

#[tauri::command]
pub fn knowledge_v2_cancel_query(query_id: String) -> Result<bool, String> {
    validate_id("query id", &query_id)?;
    let token = query_cancellations()
        .lock()
        .map_err(|_| "Knowledge query cancellation lock poisoned".to_string())?
        .get(&query_id)
        .cloned();
    if let Some(token) = token {
        token.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

struct QueryCancellationRegistration {
    query_id: String,
}

impl Drop for QueryCancellationRegistration {
    fn drop(&mut self) {
        if let Ok(mut map) = query_cancellations().lock() {
            map.remove(&self.query_id);
        }
    }
}

fn register_query_cancellation(
    query_id: &str,
) -> Result<(CancellationToken, QueryCancellationRegistration), String> {
    validate_id("query id", query_id)?;
    let token = CancellationToken::new();
    let mut map = query_cancellations()
        .lock()
        .map_err(|_| "Knowledge query cancellation lock poisoned".to_string())?;
    match map.entry(query_id.to_string()) {
        std::collections::hash_map::Entry::Occupied(_) => {
            return Err("A Knowledge query with this id is already running".to_string())
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(token.clone());
        }
    }
    Ok((
        token,
        QueryCancellationRegistration {
            query_id: query_id.to_string(),
        },
    ))
}

fn emit_progress(
    reporter: &(dyn Fn(KnowledgeRefreshProgress) + Sync),
    progress: KnowledgeRefreshProgress,
) {
    reporter(progress);
}

fn configured_ocr_document(
    object: &SourceObject,
    format: DocumentFormat,
    config: &KnowledgeOcrConfig,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Option<ExtractedDocument>, String> {
    if !config.enabled {
        return Ok(None);
    }
    let executable = config
        .executable_path
        .as_deref()
        .ok_or_else(|| "OCR is enabled without an executable".to_string())?;
    let asset = config
        .asset
        .as_ref()
        .ok_or_else(|| "OCR is enabled without asset metadata".to_string())?;
    let provider = TesseractOcrProvider::new(
        executable,
        &config.languages,
        Duration::from_secs(120),
        config.low_confidence_micros,
    )
    .map_err(|error| error.to_string())?;
    let pages = match format {
        DocumentFormat::ImageOcr => vec![OcrPageInput {
            page: 1,
            media_type: object.metadata.media_type.clone(),
            bytes: object.bytes.clone(),
        }],
        DocumentFormat::Pdf => {
            let Some(renderer) = config.pdf_renderer_path.as_deref() else {
                return Ok(None);
            };
            render_pdf_pages(renderer, object, limits, cancel)?
        }
        _ => return Ok(None),
    };
    if pages.is_empty() {
        return Ok(None);
    }
    let mut progress = |_| {};
    let blocks = run_ocr(&provider, asset, &pages, limits, cancel, &mut progress)
        .map_err(|error| error.to_string())?;
    let low_confidence = blocks
        .iter()
        .filter(|block| block.content_type == "ocr_low_confidence")
        .count();
    let document = ExtractedDocument {
        contract_version: EXTRACTOR_CONTRACT_VERSION,
        extractor_id: "sidecar.tesseract.v1".to_string(),
        extractor_version: asset.engine_version.clone(),
        source: object.metadata.clone(),
        format,
        security: DocumentSecurityDeclaration::inert(),
        blocks,
        warnings: if low_confidence > 0 {
            vec![format!(
                "{low_confidence} OCR line(s) are below the configured confidence threshold and are visibly marked"
            )]
        } else {
            Vec::new()
        },
    };
    document
        .validate(&ExtractionPolicy::default(), limits)
        .map_err(|error| error.to_string())?;
    Ok(Some(document))
}

fn render_pdf_pages(
    renderer: &str,
    object: &SourceObject,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<OcrPageInput>, String> {
    let renderer = validate_regular_executable(renderer, "PDF renderer")?;
    let temporary =
        std::env::temp_dir().join(format!("little-monkey-pdf-ocr-{}", Uuid::new_v4().simple()));
    fs::create_dir(&temporary)
        .map_err(|error| format!("Failed to create PDF OCR workspace: {error}"))?;
    let result = (|| {
        let input = temporary.join("document.pdf");
        let output_prefix = temporary.join("page");
        let stderr_path = temporary.join("renderer.stderr");
        let mut input_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&input)
            .map_err(|error| error.to_string())?;
        input_file
            .write_all(&object.bytes)
            .and_then(|()| input_file.sync_all())
            .map_err(|error| error.to_string())?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)
            .map_err(|error| error.to_string())?;
        let mut child = Command::new(renderer)
            .arg("-png")
            .arg("-r")
            .arg("300")
            .arg("-f")
            .arg("1")
            .arg("-l")
            .arg(limits.max_ocr_pages.to_string())
            .arg(&input)
            .arg(&output_prefix)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .map_err(|error| format!("Failed to start PDF renderer: {error}"))?;
        let started = Instant::now();
        let status = loop {
            if cancel.is_cancelled() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(PipelineError::Cancelled.to_string());
            }
            if started.elapsed() > Duration::from_secs(180) {
                let _ = child.kill();
                let _ = child.wait();
                return Err("PDF renderer exceeded its 180-second limit".to_string());
            }
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                break status;
            }
            thread::sleep(Duration::from_millis(25));
        };
        if !status.success() {
            let stderr = fs::read_to_string(&stderr_path)
                .unwrap_or_default()
                .chars()
                .take(2_000)
                .collect::<String>();
            return Err(format!("PDF renderer exited with {status}: {stderr}"));
        }
        let mut rendered = fs::read_dir(&temporary)
            .map_err(|error| error.to_string())?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("page-") && name.ends_with(".png"))
            })
            .collect::<Vec<_>>();
        rendered.sort_by_key(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .trim_start_matches("page-")
                .trim_end_matches(".png")
                .parse::<u32>()
                .unwrap_or(u32::MAX)
        });
        if rendered.is_empty() || rendered.len() > limits.max_ocr_pages {
            return Err("PDF renderer produced no pages or exceeded the page limit".to_string());
        }
        rendered
            .into_iter()
            .enumerate()
            .map(|(index, entry)| {
                let bytes = fs::read(entry.path()).map_err(|error| error.to_string())?;
                if bytes.len() as u64 > limits.max_file_bytes {
                    return Err("Rendered OCR page exceeds the byte limit".to_string());
                }
                Ok(OcrPageInput {
                    page: index as u32 + 1,
                    media_type: "image/png".to_string(),
                    bytes,
                })
            })
            .collect::<Result<Vec<_>, String>>()
    })();
    let _ = fs::remove_dir_all(&temporary);
    result
}

#[tauri::command]
pub async fn knowledge_v2_refresh(
    app: AppHandle,
    stack_id: String,
) -> Result<KnowledgeRefreshReport, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    let progress_app = app.clone();
    refresh_at(&app_data, &stack_id, &move |progress| {
        let _ = progress_app.emit("knowledge-v2://refresh-progress", progress);
    })
    .await
}

/// Executes the exact production connector/extraction/index publication path
/// without a Tauri window. The resident daemon uses this entry point for
/// explicitly configured unattended refreshes; it emits no UI events and
/// shares the same cancellation, catalog, and immutable-generation rules.
pub async fn knowledge_v2_refresh_headless(
    app_data: &Path,
    stack_id: &str,
) -> Result<KnowledgeRefreshReport, String> {
    refresh_at(app_data, stack_id, &|_| {}).await
}

/// Runs one daemon-owned due check. The next slot is durably advanced before
/// network/model work starts, so a daemon restart cannot immediately submit
/// the same scheduled refresh twice. Manual refresh remains available and
/// shares the exact production refresh implementation.
pub async fn run_due_background_refresh(
    app_data: &Path,
    at_ms: u64,
) -> Result<KnowledgeBackgroundRefreshOutcome, String> {
    let mut config = load_background_refresh_config_at(app_data)?;
    if !config.enabled || config.next_due_ms.is_some_and(|due| due > at_ms) {
        return Ok(KnowledgeBackgroundRefreshOutcome {
            due: false,
            refreshed_stack_ids: Vec::new(),
            failures: Vec::new(),
            next_due_ms: config.next_due_ms,
        });
    }
    let root = data_root_at(app_data)?;
    let mut stack_ids = config.stack_ids.clone();
    if stack_ids.is_empty() {
        let _guard = catalog_lock()
            .lock()
            .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
        stack_ids = load_catalog(&root)?
            .sources
            .into_iter()
            .filter(|source| source.enabled)
            .map(|source| source.stack_id)
            .collect();
        stack_ids.sort();
        stack_ids.dedup();
    }
    config.last_attempt_ms = Some(at_ms);
    config.next_due_ms = Some(at_ms.saturating_add(config.interval_minutes.saturating_mul(60_000)));
    save_background_refresh_config_at(app_data, &config)?;

    let mut refreshed_stack_ids = Vec::new();
    let mut failures = Vec::new();
    for stack_id in stack_ids {
        match knowledge_v2_refresh_headless(app_data, &stack_id).await {
            Ok(_) => refreshed_stack_ids.push(stack_id),
            Err(error) => failures.push(format!("{stack_id}: {error}")),
        }
    }
    if failures.is_empty() {
        config.last_success_ms = Some(now_ms());
        config.last_error = None;
        config.consecutive_failures = 0;
    } else {
        config.last_error = Some(
            failures
                .join("; ")
                .chars()
                .take(8 * 1024)
                .collect::<String>(),
        );
        config.consecutive_failures = config.consecutive_failures.saturating_add(1);
    }
    save_background_refresh_config_at(app_data, &config)?;
    Ok(KnowledgeBackgroundRefreshOutcome {
        due: true,
        refreshed_stack_ids,
        failures,
        next_due_ms: config.next_due_ms,
    })
}

async fn refresh_at(
    app_data: &Path,
    stack_id: &str,
    reporter: &(dyn Fn(KnowledgeRefreshProgress) + Sync),
) -> Result<KnowledgeRefreshReport, String> {
    validate_id("stack id", &stack_id)?;
    let _lease = acquire_refresh_lease(app_data, stack_id)?;
    let (cancel, _registration) = register_cancellation(&stack_id)?;
    let started = now_ms();
    let result = refresh_inner_at(app_data, stack_id, &cancel, reporter).await;
    if let Err(error) = &result {
        let root = data_root_at(app_data)?;
        if let Ok(_guard) = catalog_lock().lock() {
            if let Ok(mut catalog) = load_catalog(&root) {
                for source in catalog
                    .sources
                    .iter_mut()
                    .filter(|source| source.stack_id == stack_id)
                {
                    source.last_error = Some(error.clone());
                    source.retries.push(ConnectorRetry {
                        attempted_at_ms: now_ms(),
                        message: error.chars().take(1_000).collect(),
                    });
                    if source.retries.len() > MAX_RETRY_HISTORY {
                        source
                            .retries
                            .drain(..source.retries.len() - MAX_RETRY_HISTORY);
                    }
                }
                let _ = save_catalog(&root, &catalog);
            }
        }
    }
    result.map(|mut report| {
        report.duration_ms = now_ms().saturating_sub(started);
        report
    })
}

async fn refresh_inner_at(
    app_data: &Path,
    stack_id: &str,
    cancel: &CancellationToken,
    reporter: &(dyn Fn(KnowledgeRefreshProgress) + Sync),
) -> Result<KnowledgeRefreshReport, String> {
    let root = data_root_at(app_data)?;
    let stacks_root = app_data.join("stacks");
    let stack = crate::stacks::list_impl(&stacks_root)?
        .into_iter()
        .find(|stack| stack.id == stack_id)
        .ok_or_else(|| "Knowledge stack not found".to_string())?;
    let sources = {
        let _guard = catalog_lock()
            .lock()
            .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
        load_catalog(&root)?
            .sources
            .into_iter()
            .filter(|source| source.stack_id == stack_id && source.enabled)
            .collect::<Vec<_>>()
    };
    if sources.is_empty() {
        return Err("Add and enable at least one Knowledge 2.0 source".to_string());
    }
    emit_progress(
        reporter,
        KnowledgeRefreshProgress {
            stack_id: stack_id.to_string(),
            source_id: None,
            phase: "enumerating".to_string(),
            objects_done: 0,
            objects_total: 0,
            chunks: 0,
            reused_chunks: 0,
        },
    );
    let limits = PipelineLimits::default();
    let mut collected = Vec::<(KnowledgeSource, Vec<SourceObject>)>::new();
    // Some connectors (GitHub/S3/Notion/Slack/Jira) resolve a REAL upstream
    // incremental cursor (a commit SHA, an ETag map, a high-water timestamp,
    // ...) during collection rather than relying solely on the generic
    // content-hash cursor computed below — see `collect_source_objects`'s doc
    // comment. Collected here, keyed by source id, and preferred over the
    // generic cursor in the catalog-write block near the end of this
    // function.
    let mut explicit_cursors = HashMap::<String, String>::new();
    for source in sources {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        emit_progress(
            reporter,
            KnowledgeRefreshProgress {
                stack_id: stack_id.to_string(),
                source_id: Some(source.id.clone()),
                phase: "fetching".to_string(),
                objects_done: collected.iter().map(|(_, objects)| objects.len()).sum(),
                objects_total: 0,
                chunks: 0,
                reused_chunks: 0,
            },
        );
        let (objects, explicit_cursor) =
            collect_source_objects(app_data, &source, &limits, cancel).await?;
        if let Some(cursor) = explicit_cursor {
            explicit_cursors.insert(source.id.clone(), cursor);
        }
        collected.push((source, objects));
    }
    let object_total = collected
        .iter()
        .map(|(_, objects)| objects.len())
        .sum::<usize>();
    if object_total
        > limits
            .max_objects_per_source
            .saturating_mul(collected.len())
    {
        return Err("Knowledge source set exceeds the configured object limit".to_string());
    }
    let embedding = pipeline_embedding(&stack);
    let ocr_config = load_ocr_config(&root)?;
    let chunking = ChunkingSpec {
        strategy_version: crate::knowledge_pipeline::CHUNKER_CONTRACT_VERSION,
        target_chars: stack.chunk_chars.max(64),
        overlap_chars: stack.chunk_overlap.min(stack.chunk_chars.saturating_sub(1)),
        min_chars: 40.min(stack.chunk_chars.max(1)),
    };
    let pipeline_fingerprint = sha256(
        &serde_json::to_vec(&serde_json::json!({
            "extractors": ["plain-text-v1", "office-openxml-v1", "html-pdf-v1"],
            "chunking": chunking,
            "embedding": embedding,
            "privacy": "local-default",
            "ocr": &ocr_config,
        }))
        .map_err(|error| error.to_string())?,
    );
    let generation_store =
        GenerationStore::new(root.join("indexes")).map_err(|error| error.to_string())?;
    let active = generation_store
        .active(stack_id)
        .map_err(|error| error.to_string())?;
    let previous_snapshots = active
        .as_ref()
        .map(|active| active.manifest.objects.clone())
        .unwrap_or_default();
    let previous_entries = generation_store
        .open_active_index(stack_id)
        .map_err(|error| error.to_string())?
        .map(|index| index.entries())
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    let previous_chunks = previous_entries
        .into_iter()
        .map(|(chunk, vector)| (chunk.chunk_id.clone(), (chunk, vector)))
        .collect::<HashMap<_, _>>();
    let previous_by_object = previous_snapshots
        .iter()
        .map(|snapshot| {
            (
                (snapshot.source_id.clone(), snapshot.object_id.clone()),
                snapshot,
            )
        })
        .collect::<HashMap<_, _>>();

    let mut registry = ExtractorRegistry::default();
    registry
        .register(Box::new(crate::knowledge_pipeline::PlainTextExtractor))
        .map_err(|error| error.to_string())?;
    registry
        .register(Box::new(OfficeOpenXmlExtractor))
        .map_err(|error| error.to_string())?;
    registry
        .register(Box::new(HtmlPdfExtractor))
        .map_err(|error| error.to_string())?;
    let chunker = LocationAwareChunker;
    let policy = ExtractionPolicy::default();
    let mut snapshots = Vec::new();
    let mut final_chunks = Vec::<KnowledgeChunk>::new();
    let mut final_vectors = Vec::<Vec<f32>>::new();
    let mut pending_chunks = Vec::<KnowledgeChunk>::new();
    let mut changed_objects = 0;
    let mut unchanged_objects = 0;
    let mut warnings = Vec::new();
    let mut source_states = HashMap::<String, Vec<ConnectorObjectState>>::new();

    let mut objects_done = 0;
    for (source, objects) in &collected {
        for object in objects {
            if cancel.is_cancelled() {
                return Err(PipelineError::Cancelled.to_string());
            }
            let key = (
                object.metadata.source_id.clone(),
                object.metadata.object_id.clone(),
            );
            let reusable = previous_by_object.get(&key).is_some_and(|snapshot| {
                snapshot.content_sha256 == object.metadata.content_sha256
                    && snapshot.pipeline_fingerprint == pipeline_fingerprint
                    && snapshot
                        .chunk_ids
                        .iter()
                        .all(|id| previous_chunks.contains_key(id))
            });
            let chunk_ids =
                if reusable {
                    unchanged_objects += 1;
                    let snapshot = previous_by_object[&key];
                    for chunk_id in &snapshot.chunk_ids {
                        let (chunk, vector) = previous_chunks[chunk_id].clone();
                        final_chunks.push(chunk);
                        final_vectors.push(vector);
                    }
                    snapshot.chunk_ids.clone()
                } else {
                    changed_objects += 1;
                    let format = DocumentFormat::from_media_type(&object.metadata.media_type)
                        .ok_or_else(|| {
                            format!("Unsupported media type: {}", object.metadata.media_type)
                        })?;
                    let mut document = if format == DocumentFormat::ImageOcr {
                        configured_ocr_document(object, format, &ocr_config, &limits, cancel)?
                    } else {
                        Some(
                            registry
                                .extract(object, &policy, &limits, cancel)
                                .map_err(|error| error.to_string())?,
                        )
                    };
                    if format == DocumentFormat::Pdf
                        && document
                            .as_ref()
                            .is_some_and(|document| document.blocks.is_empty())
                    {
                        if let Some(ocr_document) =
                            configured_ocr_document(object, format, &ocr_config, &limits, cancel)?
                        {
                            document = Some(ocr_document);
                        }
                    }
                    if let Some(document) = document {
                        warnings.extend(document.warnings.iter().map(|warning| {
                            format!("{}: {warning}", object.metadata.canonical_uri)
                        }));
                        if document.blocks.is_empty() {
                            warnings.push(format!(
                                "{} produced no text; configure local OCR for scanned content",
                                object.metadata.canonical_uri
                            ));
                        }
                        let chunks = chunker
                            .chunk(&document, &chunking, &limits, cancel)
                            .map_err(|error| error.to_string())?;
                        let ids = chunks.iter().map(|chunk| chunk.chunk_id.clone()).collect();
                        pending_chunks.extend(chunks);
                        ids
                    } else {
                        warnings.push(format!(
                            "{} is an image and local OCR is not configured; it was not indexed",
                            object.metadata.canonical_uri
                        ));
                        Vec::new()
                    }
                };
            snapshots.push(ObjectSnapshot {
                source_id: object.metadata.source_id.clone(),
                object_id: object.metadata.object_id.clone(),
                content_sha256: object.metadata.content_sha256.clone(),
                pipeline_fingerprint: pipeline_fingerprint.clone(),
                chunk_ids: chunk_ids.clone(),
            });
            source_states
                .entry(source.id.clone())
                .or_default()
                .push(ConnectorObjectState {
                    object_id: object.metadata.object_id.clone(),
                    canonical_uri: object.metadata.canonical_uri.clone(),
                    content_sha256: object.metadata.content_sha256.clone(),
                    etag: object.metadata.etag.clone(),
                    modified_unix_ms: object.metadata.modified_unix_ms,
                    chunk_ids,
                });
            objects_done += 1;
            emit_progress(
                reporter,
                KnowledgeRefreshProgress {
                    stack_id: stack_id.to_string(),
                    source_id: Some(source.id.clone()),
                    phase: "extracting".to_string(),
                    objects_done,
                    objects_total: object_total,
                    chunks: final_chunks.len() + pending_chunks.len(),
                    reused_chunks: final_chunks.len(),
                },
            );
        }
    }
    let embedded_chunks = pending_chunks.len();
    if !pending_chunks.is_empty() {
        emit_progress(
            reporter,
            KnowledgeRefreshProgress {
                stack_id: stack_id.to_string(),
                source_id: None,
                phase: "embedding".to_string(),
                objects_done,
                objects_total: object_total,
                chunks: final_chunks.len() + pending_chunks.len(),
                reused_chunks: final_chunks.len(),
            },
        );
        let texts = pending_chunks
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>();
        let vectors = crate::stacks::embed_batch(&stack.embedding, &texts, false).await?;
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        final_chunks.extend(pending_chunks);
        final_vectors.extend(vectors);
    }
    let reused_chunks = final_chunks.len().saturating_sub(embedded_chunks);
    let generation_id = Uuid::new_v4().to_string();
    let parent_generation_id = active
        .as_ref()
        .map(|active| active.manifest.generation_id.clone());
    let build = GenerationBuild {
        draft: GenerationDraft {
            stack_id: stack_id.to_string(),
            generation_id: generation_id.clone(),
            parent_generation_id: parent_generation_id.clone(),
            created_unix_ms: now_ms(),
            pipeline_fingerprint,
            embedding_spec: embedding,
            objects: snapshots,
        },
        chunks: final_chunks,
        vectors: final_vectors,
    };
    emit_progress(
        reporter,
        KnowledgeRefreshProgress {
            stack_id: stack_id.to_string(),
            source_id: None,
            phase: "publishing".to_string(),
            objects_done,
            objects_total: object_total,
            chunks: build.chunks.len(),
            reused_chunks,
        },
    );
    let staged = generation_store
        .stage(&build, &limits, cancel)
        .map_err(|error| error.to_string())?;
    generation_store
        .activate(staged, cancel)
        .map_err(|error| error.to_string())?;
    if let Err(error) =
        crate::stacks::mark_v2_indexed_impl(&stacks_root, stack_id, now_ms(), build.chunks.len())
    {
        warnings.push(format!(
            "The v2 index is active, but its legacy readiness badge could not be updated: {error}"
        ));
    }

    let completed_at = now_ms();
    {
        let _guard = catalog_lock()
            .lock()
            .map_err(|_| "Knowledge catalog lock poisoned".to_string())?;
        let mut catalog = load_catalog(&root)?;
        for source in catalog
            .sources
            .iter_mut()
            .filter(|source| source.stack_id == stack_id)
        {
            if let Some(objects) = source_states.remove(&source.id) {
                source.cursor = match explicit_cursors.remove(&source.id) {
                    Some(cursor) => Some(cursor),
                    None => {
                        let cursor_payload = objects
                            .iter()
                            .map(|object| (&object.object_id, &object.content_sha256))
                            .collect::<Vec<_>>();
                        Some(sha256(
                            &serde_json::to_vec(&cursor_payload)
                                .map_err(|error| error.to_string())?,
                        ))
                    }
                };
                source.checkpoint = Some(generation_id.clone());
                source.last_refresh_at_ms = Some(completed_at);
                source.last_error = None;
                source.objects = objects;
            }
        }
        save_catalog(&root, &catalog)?;
    }
    emit_progress(
        reporter,
        KnowledgeRefreshProgress {
            stack_id: stack_id.to_string(),
            source_id: None,
            phase: "done".to_string(),
            objects_done,
            objects_total: object_total,
            chunks: build.chunks.len(),
            reused_chunks,
        },
    );
    let previous_keys = previous_snapshots
        .iter()
        .map(|snapshot| (snapshot.source_id.as_str(), snapshot.object_id.as_str()))
        .collect::<BTreeSet<_>>();
    let current_keys = build
        .draft
        .objects
        .iter()
        .map(|snapshot| (snapshot.source_id.as_str(), snapshot.object_id.as_str()))
        .collect::<BTreeSet<_>>();
    Ok(KnowledgeRefreshReport {
        stack_id: stack_id.to_string(),
        generation_id,
        parent_generation_id,
        source_count: collected.len(),
        object_count: object_total,
        changed_objects,
        unchanged_objects,
        deleted_objects: previous_keys.difference(&current_keys).count(),
        embedded_chunks,
        reused_chunks,
        warnings,
        duration_ms: 0,
    })
}

fn pipeline_embedding(stack: &KnowledgeStack) -> PipelineEmbeddingSpec {
    PipelineEmbeddingSpec {
        contract_version: EMBEDDING_CONTRACT_VERSION,
        provider_id: match stack.embedding.backend {
            EmbeddingBackend::Llama => "local.llama-cpp".to_string(),
            EmbeddingBackend::Ollama => "local.ollama".to_string(),
        },
        model_id: stack.embedding.model_id_or_tag.clone(),
        dimension: stack.embedding.dim as usize,
        query_prefix: stack.embedding.query_prefix.clone(),
        document_prefix: stack.embedding.doc_prefix.clone(),
        normalized: true,
    }
}

/// Collects `source`'s current object set. Returns `(objects, explicit_cursor)`:
/// `explicit_cursor`, when `Some`, is a REAL upstream incremental cursor the
/// connector itself resolved (a commit SHA, an ETag map, a high-water
/// timestamp, ...) and takes priority over `refresh_inner_at`'s generic
/// content-hash cursor (see that function's doc comment on
/// `explicit_cursors`). `None` means "no such cursor — fall back to the
/// generic one", which is every pre-existing connector kind's behavior
/// unchanged.
async fn collect_source_objects(
    app_data: &Path,
    source: &KnowledgeSource,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    match &source.connector {
        ConnectorConfig::LocalFile { path } => {
            collect_local_file(source, Path::new(path), limits).map(|objects| (objects, None))
        }
        ConnectorConfig::LocalFolder { path } | ConnectorConfig::Project { path } => {
            collect_local_folder(source, Path::new(path), limits, cancel)
                .map(|objects| (objects, None))
        }
        ConnectorConfig::WatchedFolder { path, .. } => {
            collect_local_folder(source, Path::new(path), limits, cancel)
                .map(|objects| (objects, None))
        }
        ConnectorConfig::GitHubRepo {
            owner,
            repo,
            git_ref,
            path_prefix,
            connector_account_id,
        } => {
            collect_github_repo(
                app_data,
                source,
                owner,
                repo,
                git_ref.as_deref(),
                path_prefix.as_deref(),
                connector_account_id,
                limits,
                cancel,
            )
            .await
        }
        ConnectorConfig::S3Bucket {
            endpoint,
            bucket,
            prefix,
            region,
            connector_account_id,
        } => {
            collect_s3_bucket(
                app_data,
                source,
                endpoint,
                bucket,
                prefix.as_deref(),
                region,
                connector_account_id,
                limits,
                cancel,
            )
            .await
        }
        ConnectorConfig::NotionPages {
            connector_account_id,
            root_id,
        } => {
            collect_notion_pages(app_data, source, connector_account_id, root_id, limits, cancel)
                .await
        }
        ConnectorConfig::SlackChannels {
            connector_account_id,
            channel_ids,
        } => {
            collect_slack_channels(
                app_data,
                source,
                connector_account_id,
                channel_ids,
                limits,
                cancel,
            )
            .await
        }
        ConnectorConfig::JiraProject {
            connector_account_id,
            project_key,
        } => {
            collect_jira_project(
                app_data,
                source,
                connector_account_id,
                project_key,
                limits,
                cancel,
            )
            .await
        }
        ConnectorConfig::Url {
            url,
            allowed_origin,
            max_depth,
            max_pages,
            obey_robots,
            allow_loopback,
        } => {
            crawl_url(
                source,
                url,
                allowed_origin,
                *max_depth,
                *max_pages,
                *obey_robots,
                *allow_loopback,
                None,
                limits,
                cancel,
            )
            .await
            .map(|objects| (objects, None))
        }
        ConnectorConfig::Sitemap {
            url,
            allowed_origin,
            max_pages,
            obey_robots,
            allow_loopback,
        } => {
            collect_sitemap(
                source,
                url,
                allowed_origin,
                *max_pages,
                *obey_robots,
                *allow_loopback,
                limits,
                cancel,
            )
            .await
            .map(|objects| (objects, None))
        }
        ConnectorConfig::SelectedChats { session_ids } => {
            collect_selected_chats(app_data, source, session_ids, limits).map(|objects| (objects, None))
        }
        ConnectorConfig::WebDav {
            url,
            username,
            credential_ref,
            allow_loopback,
        } => {
            let password = keyring::Entry::new("little-monkey-knowledge-webdav", credential_ref)
                .map_err(|error| format!("Failed to access WebDAV keychain item: {error}"))?
                .get_password()
                .map_err(|error| format!("WebDAV credential is unavailable: {error}"))?;
            let parsed = Url::parse(url).map_err(|error| error.to_string())?;
            let origin = origin_of(&parsed)?;
            let object = fetch_validated(
                source,
                url,
                &origin,
                *allow_loopback,
                Some((username.as_str(), password.as_str())),
                limits,
                cancel,
            )
            .await?;
            Ok((vec![object], None))
        }
    }
}

// =============================================================================
// External Knowledge Sync connectors (ROADMAP "External Knowledge Sync
// Pipelines"): GitHubRepo, S3Bucket, WatchedFolder, NotionPages,
// SlackChannels, JiraProject.
//
// Non-goals — see `ConnectorConfig`'s doc comment above for the full
// rationale: SharePoint and Google Drive are not implemented (both require a
// registered OAuth app, out of scope for this build); GitLab is a stretch
// left for later, not implemented in this pass.
// =============================================================================

// --- shared: local object-content cache --------------------------------------
//
// GitHubRepo/S3Bucket/NotionPages/SlackChannels/JiraProject each resolve a
// REAL upstream incremental cursor (a commit SHA, an ETag map, a
// last-edited-time high-water mark, ...) and use it to skip re-fetching
// objects their own cursor says are unchanged. Skipping the fetch still
// needs to produce a `SourceObject` with the right bytes for the extraction/
// chunking pipeline below, so unchanged objects are read back from this
// on-disk cache (written on every successful fetch) instead of the network —
// never used for secrets, only the same object content the pipeline would
// otherwise download and chunk anyway.
fn connector_cache_dir(app_data: &Path, source_id: &str) -> Result<PathBuf, String> {
    validate_id("source id", source_id)?;
    let dir = data_root_at(app_data)?
        .join("connector-cache")
        .join(source_id);
    fs::create_dir_all(&dir).map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
    Ok(dir)
}

fn connector_cache_path(cache_dir: &Path, object_id: &str) -> PathBuf {
    cache_dir.join(format!("{}.bin", sha256(object_id.as_bytes())))
}

fn connector_cache_read(cache_dir: &Path, object_id: &str) -> Option<Vec<u8>> {
    fs::read(connector_cache_path(cache_dir, object_id)).ok()
}

fn connector_cache_write(cache_dir: &Path, object_id: &str, bytes: &[u8]) -> Result<(), String> {
    let path = connector_cache_path(cache_dir, object_id);
    let temporary = cache_dir.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    fs::write(&temporary, bytes)
        .map_err(|error| format!("Failed to write connector cache entry: {error}"))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("Failed to publish connector cache entry: {error}"))?;
    Ok(())
}

// --- shared: SSRF-hardened authenticated fetch -------------------------------

/// Like `fetch_http`, but for the network-incremental connectors below:
/// generic bearer/basic-auth-style headers instead of just basic auth, no
/// redirect following (none of GitHub/S3/Notion/Slack/Jira's read endpoints
/// legitimately redirect — same posture as `connectors.rs::verified_call`),
/// and it returns the response headers too (S3's `GetObject` doesn't need
/// them, but callers that do — none yet — can). DNS is resolved once and
/// pinned to the exact socket used, non-public/loopback addresses are always
/// rejected (mirrors `connectors.rs`'s stance: these are trusted, either
/// fixed well-known API hosts or a user-supplied S3 endpoint/Jira site
/// pinned to its own origin), and the response is capped and streamed the
/// same way `fetch_http` is. `allow_loopback` exists only so this file's own
/// tests can point it at a local fixture server — every production call site
/// below passes `false`, exactly like `connectors.rs::verified_call`.
async fn fetch_connector_bytes(
    method: reqwest::Method,
    url: &str,
    allowed_origin: &str,
    allow_loopback: bool,
    headers: &[(&'static str, String)],
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, reqwest::header::HeaderMap), String> {
    let policy = UrlSourcePolicy::new([allowed_origin], allow_loopback, false)
        .map_err(|error| error.to_string())?;
    let parsed = Url::parse(url).map_err(|error| error.to_string())?;
    let addresses = resolve_url(&parsed).await?;
    policy
        .validate(url, &addresses, limits)
        .map_err(|error| error.to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let socket = SocketAddr::new(addresses[0], port);
    let client = pinned_http_client(host, socket)?;
    let mut request = client.request(method, parsed.clone());
    for (key, value) in headers {
        request = request.header(*key, value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Connector request failed: {error}"))?;
    if response.status().is_redirection() {
        return Err("Connector response was a redirect — refusing to follow".to_string());
    }
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "Connector request failed with HTTP {status}: {}",
            body.chars().take(500).collect::<String>()
        ));
    }
    let response_headers = response.headers().clone();
    if let Some(length) = response.content_length() {
        if length > limits.max_file_bytes.min(MAX_HTTP_BYTES as u64) {
            return Err("Connector response exceeds the byte limit".to_string());
        }
    }
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let chunk = chunk.map_err(|error| format!("Connector response stream failed: {error}"))?;
        if bytes.len().saturating_add(chunk.len())
            > limits.max_file_bytes.min(MAX_HTTP_BYTES as u64) as usize
        {
            return Err("Connector response exceeds the byte limit".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok((bytes, response_headers))
}

/// `application/x-www-form-urlencoded`-style percent-encoding for one query
/// parameter *value* (Notion's `start_cursor`, Slack's `oldest`/`cursor`,
/// Jira's `jql`) — distinct from `connectors::sigv4_uri_encode`, which is
/// RFC 3986 encoding for SigV4's stricter canonical-request rules.
fn percent_encode_query(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

/// Parses both Notion's `...Z` (Zulu) and Jira's `...+0000` (no-colon
/// offset) ISO-8601 timestamp flavors into Unix milliseconds — `rfc3339`
/// handles the former, the explicit format string handles the latter (which
/// `parse_from_rfc3339` rejects outright).
fn parse_iso8601_ms(value: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .or_else(|| chrono::DateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.3f%z").ok())
        .and_then(|datetime| u64::try_from(datetime.timestamp_millis()).ok())
}

// --- GitHubRepo ---------------------------------------------------------------

async fn gh_api_json_call(path: String) -> Result<Value, String> {
    tokio::task::spawn_blocking(move || crate::m5_delivery::m5_github_api_get(&path))
        .await
        .map_err(|error| format!("GitHub CLI task failed: {error}"))?
}

fn github_path_allowed(path: &str, prefix: Option<&str>) -> bool {
    let prefix_ok = match prefix {
        Some(prefix) if !prefix.is_empty() => path.starts_with(prefix.trim_end_matches('/')),
        _ => true,
    };
    prefix_ok && media_type_for_path(Path::new(path)).is_some()
}

/// What [`collect_github_repo`] learned about this refresh's commit range,
/// already reduced to plain JSON so [`github_plan_paths`] (the pure
/// dedup/incremental-diff decision below) never touches the network/`gh`
/// process itself and is directly unit-testable with fixture JSON.
enum GithubCursorState {
    /// `source.cursor` already equals the current commit sha — nothing to
    /// diff, replay the previous path set verbatim.
    Unchanged,
    /// `repos/{owner}/{repo}/compare/{cursor}...{current}`'s `files` array.
    Compare(Vec<Value>),
    /// `repos/{owner}/{repo}/git/trees/{current}?recursive=1`'s `tree` array
    /// — used on this source's first-ever refresh, or whenever the previous
    /// path set is empty (e.g. after a `git_ref` change invalidated it).
    FullTree(Vec<Value>),
}

/// Pure dedup/incremental-diff planner for [`collect_github_repo`]: given the
/// previous refresh's known paths and this refresh's already-fetched cursor
/// state, returns `(all_current_paths, changed_paths)` — `changed_paths` is
/// the subset that actually needs a fresh `GetObject`/contents fetch;
/// everything else in `all_current_paths` is expected to already be sitting
/// in the local connector cache from a previous refresh.
fn github_plan_paths(
    previous_paths: BTreeSet<String>,
    state: GithubCursorState,
    path_prefix: Option<&str>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut changed_paths = BTreeSet::<String>::new();
    let all_paths = match state {
        GithubCursorState::Unchanged => previous_paths,
        GithubCursorState::Compare(files) => {
            let mut paths = previous_paths;
            for file in &files {
                let Some(filename) = file.get("filename").and_then(Value::as_str) else {
                    continue;
                };
                let status = file.get("status").and_then(Value::as_str).unwrap_or_default();
                if let Some(previous_name) = file.get("previous_filename").and_then(Value::as_str) {
                    paths.remove(previous_name);
                }
                match status {
                    "removed" => {
                        paths.remove(filename);
                    }
                    _ if github_path_allowed(filename, path_prefix) => {
                        paths.insert(filename.to_string());
                        changed_paths.insert(filename.to_string());
                    }
                    _ => {}
                }
            }
            paths
        }
        GithubCursorState::FullTree(entries) => {
            let mut paths = BTreeSet::new();
            for entry in &entries {
                if entry.get("type").and_then(Value::as_str) != Some("blob") {
                    continue;
                }
                let Some(path) = entry.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if !github_path_allowed(path, path_prefix) {
                    continue;
                }
                paths.insert(path.to_string());
                changed_paths.insert(path.to_string());
            }
            paths
        }
    };
    (all_paths, changed_paths)
}

/// Result of a single GitHub Contents-API fetch: GitHub only inlines
/// base64 `content` for blobs under ~1MB — larger files omit `content`
/// entirely (returning `git_url`/`size` instead), which must be treated as
/// "skip this file" rather than a hard error, or a single oversized file
/// anywhere in the tree would abort the refresh of every source in the stack.
enum GhFileContent {
    Inline(Vec<u8>),
    TooLargeForContentsApi,
}

async fn gh_fetch_file_bytes(
    owner: &str,
    repo: &str,
    path: &str,
    at_ref: &str,
) -> Result<GhFileContent, String> {
    let encoded_path = connectors::sigv4_uri_encode(path, false);
    let json =
        gh_api_json_call(format!("repos/{owner}/{repo}/contents/{encoded_path}?ref={at_ref}")).await?;
    let Some(encoded) = json.get("content").and_then(Value::as_str) else {
        return Ok(GhFileContent::TooLargeForContentsApi);
    };
    let cleaned: String = encoded.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD
        .decode(cleaned.as_bytes())
        .map(GhFileContent::Inline)
        .map_err(|error| format!("Failed to decode GitHub file content for {path}: {error}"))
}

/// Real incremental GitHub sync: `source.cursor` is the last-seen commit SHA
/// for `git_ref` (or the repo's default branch, resolved as `HEAD`, if
/// unset). On a repeat refresh whose ref has moved, only the files GitHub's
/// own `compare` API reports as changed since that SHA are re-fetched;
/// everything else is replayed from `connector_cache_read` — a full
/// `git/trees` listing only ever runs once, on this source's first refresh
/// (or after a ref change invalidates the previous path set).
#[allow(clippy::too_many_arguments)]
async fn collect_github_repo(
    app_data: &Path,
    source: &KnowledgeSource,
    owner: &str,
    repo: &str,
    git_ref: Option<&str>,
    path_prefix: Option<&str>,
    connector_account_id: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    let account = connectors::account_by_id(connector_account_id)?;
    if account.provider != connectors::ConnectorProvider::Github {
        return Err("The selected connector account is not a GitHub account".to_string());
    }
    let git_ref_display = git_ref.unwrap_or("HEAD").to_string();
    let commit = gh_api_json_call(format!("repos/{owner}/{repo}/commits/{git_ref_display}")).await?;
    let current_sha = commit
        .get("sha")
        .and_then(Value::as_str)
        .ok_or_else(|| "GitHub commit lookup returned no sha".to_string())?
        .to_string();
    if cancel.is_cancelled() {
        return Err(PipelineError::Cancelled.to_string());
    }

    let uri_prefix = format!("https://github.com/{owner}/{repo}/blob/{git_ref_display}/");
    let previous_paths: BTreeSet<String> = source
        .objects
        .iter()
        .filter_map(|object| object.canonical_uri.strip_prefix(uri_prefix.as_str()).map(str::to_string))
        .collect();

    let cursor_state = match source.cursor.as_deref() {
        Some(prev_sha) if prev_sha == current_sha => GithubCursorState::Unchanged,
        Some(prev_sha) if !previous_paths.is_empty() => {
            let compare =
                gh_api_json_call(format!("repos/{owner}/{repo}/compare/{prev_sha}...{current_sha}"))
                    .await?;
            GithubCursorState::Compare(
                compare
                    .get("files")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            )
        }
        _ => {
            let tree = gh_api_json_call(format!(
                "repos/{owner}/{repo}/git/trees/{current_sha}?recursive=1"
            ))
            .await?;
            GithubCursorState::FullTree(
                tree.get("tree").and_then(Value::as_array).cloned().unwrap_or_default(),
            )
        }
    };
    let (all_paths, changed_paths) = github_plan_paths(previous_paths, cursor_state, path_prefix);

    if all_paths.len() > limits.max_objects_per_source {
        return Err(
            "GitHub repository has more indexable files than the configured object limit"
                .to_string(),
        );
    }

    let cache_dir = connector_cache_dir(app_data, &source.id)?;
    let mut objects = Vec::new();
    for path in &all_paths {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let Some(media_type) = media_type_for_path(Path::new(path)) else {
            continue;
        };
        let object_id = format!("gh-{}", &sha256(format!("{owner}/{repo}/{path}").as_bytes())[..32]);
        let canonical_uri = format!("{uri_prefix}{path}");
        let bytes = if changed_paths.contains(path) {
            None
        } else {
            connector_cache_read(&cache_dir, &object_id)
        };
        let bytes = match bytes {
            Some(bytes) => bytes,
            None => match gh_fetch_file_bytes(owner, repo, path, &current_sha).await? {
                GhFileContent::TooLargeForContentsApi => continue,
                GhFileContent::Inline(fetched) => {
                    if fetched.len() as u64 > limits.max_file_bytes {
                        continue;
                    }
                    connector_cache_write(&cache_dir, &object_id, &fetched)?;
                    fetched
                }
            },
        };
        objects.push(source_object_from_bytes(
            &source.id,
            &object_id,
            canonical_uri,
            media_type.to_string(),
            bytes,
            None,
            None,
        ));
    }
    Ok((objects, Some(current_sha)))
}

// --- S3Bucket -----------------------------------------------------------------

struct S3ListPage {
    entries: Vec<(String, String, u64)>,
    is_truncated: bool,
    next_token: Option<String>,
}

/// Minimal `ListObjectsV2` XML response parser — enough of AWS/R2/MinIO's
/// standard shape (`Contents/{Key,ETag,Size}`, `IsTruncated`,
/// `NextContinuationToken`) to drive pagination and per-key ETag tracking,
/// without a full XML-schema/namespace-aware parser.
fn parse_list_objects_v2(bytes: &[u8]) -> Result<S3ListPage, String> {
    let mut reader = quick_xml::Reader::from_reader(bytes);
    reader.config_mut().trim_text(true);
    let mut entries = Vec::new();
    let mut is_truncated = false;
    let mut next_token = None;
    let mut in_contents = false;
    let mut current_key: Option<String> = None;
    let mut current_etag: Option<String> = None;
    let mut current_size: Option<u64> = None;
    let mut current_tag = String::new();
    loop {
        match reader.read_event().map_err(|error| error.to_string())? {
            quick_xml::events::Event::Start(start) => {
                let name = String::from_utf8_lossy(start.name().as_ref()).to_string();
                if name == "Contents" {
                    in_contents = true;
                    current_key = None;
                    current_etag = None;
                    current_size = None;
                }
                current_tag = name;
            }
            quick_xml::events::Event::Text(text) => {
                let value = text.decode().map_err(|error| error.to_string())?.to_string();
                if in_contents {
                    match current_tag.as_str() {
                        "Key" => current_key = Some(value),
                        "ETag" => current_etag = Some(value.trim_matches('"').to_string()),
                        "Size" => current_size = value.parse::<u64>().ok(),
                        _ => {}
                    }
                } else {
                    match current_tag.as_str() {
                        "IsTruncated" => is_truncated = value == "true",
                        "NextContinuationToken" => next_token = Some(value),
                        _ => {}
                    }
                }
            }
            quick_xml::events::Event::End(end) => {
                let name = String::from_utf8_lossy(end.name().as_ref()).to_string();
                if name == "Contents" {
                    if let (Some(key), Some(etag)) = (current_key.take(), current_etag.take()) {
                        entries.push((key, etag, current_size.take().unwrap_or(0)));
                    }
                    in_contents = false;
                }
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    Ok(S3ListPage {
        entries,
        is_truncated,
        next_token,
    })
}

/// Pure incremental-diff planner for [`collect_s3_bucket`]: given the
/// previous refresh's per-key ETag map and this refresh's freshly listed
/// `(key, etag, size)` entries, returns `(current_etags, changed_keys)` —
/// `changed_keys` is the subset whose ETag differs from last time (or is
/// new) and therefore needs a fresh `GetObject`; every other listed key is
/// expected to already be sitting in the local connector cache.
fn s3_plan_changed_keys(
    previous_etags: &HashMap<String, String>,
    listed: &[(String, String, u64)],
    max_file_bytes: u64,
) -> (HashMap<String, String>, BTreeSet<String>) {
    let mut current = HashMap::new();
    let mut changed = BTreeSet::new();
    for (key, etag, size) in listed {
        if *size > max_file_bytes || media_type_for_path(Path::new(key)).is_none() {
            continue;
        }
        current.insert(key.clone(), etag.clone());
        if previous_etags.get(key) != Some(etag) {
            changed.insert(key.clone());
        }
    }
    (current, changed)
}

#[allow(clippy::too_many_arguments)]
async fn s3_signed_get(
    endpoint_url: &Url,
    canonical_uri: &str,
    query_pairs: &[(&str, &str)],
    access_key: &str,
    secret_key: &str,
    region: &str,
    allow_loopback: bool,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let host_header = connectors::host_header_value(endpoint_url)?;
    let canonical_query = connectors::sigv4_canonical_query(query_pairs);
    let headers = connectors::sigv4_signed_headers(
        "GET",
        &host_header,
        canonical_uri,
        &canonical_query,
        access_key,
        secret_key,
        region,
    );
    let mut full_url = format!("{}://{host_header}{canonical_uri}", endpoint_url.scheme());
    if !canonical_query.is_empty() {
        full_url.push('?');
        full_url.push_str(&canonical_query);
    }
    let origin = origin_of(endpoint_url)?;
    let (bytes, _headers) = fetch_connector_bytes(
        reqwest::Method::GET,
        &full_url,
        &origin,
        allow_loopback,
        &headers,
        limits,
        cancel,
    )
    .await?;
    Ok(bytes)
}

/// Real incremental S3/R2/MinIO sync: `source.cursor` is a JSON
/// `{ key: etag }` map. Every refresh still lists the bucket/prefix (S3 has
/// no "changed since" listing API), but only keys whose ETag differs from
/// last time are actually fetched with `GetObject` — everything else is
/// replayed from `connector_cache_read`.
#[allow(clippy::too_many_arguments)]
async fn collect_s3_bucket(
    app_data: &Path,
    source: &KnowledgeSource,
    endpoint: &str,
    bucket: &str,
    prefix: Option<&str>,
    region: &str,
    connector_account_id: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    let account = connectors::account_by_id(connector_account_id)?;
    if account.provider != connectors::ConnectorProvider::S3 {
        return Err("The selected connector account is not an S3 account".to_string());
    }
    let secret_key = connectors::credential_for_account(&account)?;
    let (_, _, _, access_key) = connectors::s3_connection(&account)?;
    let endpoint_url = Url::parse(endpoint).map_err(|error| format!("Invalid S3 endpoint: {error}"))?;
    let prefix = prefix.unwrap_or("").to_string();

    let previous_etags: HashMap<String, String> = source
        .cursor
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();

    let cache_dir = connector_cache_dir(app_data, &source.id)?;
    let mut listed = Vec::<(String, String, u64)>::new();
    let mut continuation: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let mut query_pairs: Vec<(&str, &str)> = vec![("list-type", "2"), ("max-keys", "1000")];
        if !prefix.is_empty() {
            query_pairs.push(("prefix", &prefix));
        }
        if let Some(token) = continuation.as_deref() {
            query_pairs.push(("continuation-token", token));
        }
        let canonical_uri = format!("/{bucket}");
        let bytes = s3_signed_get(
            &endpoint_url,
            &canonical_uri,
            &query_pairs,
            &access_key,
            &secret_key,
            region,
            false,
            limits,
            cancel,
        )
        .await?;
        let page = parse_list_objects_v2(&bytes)?;
        listed.extend(page.entries);
        if listed.len() > limits.max_objects_per_source {
            return Err("S3 bucket/prefix has more objects than the configured limit".to_string());
        }
        if page.is_truncated {
            match page.next_token {
                Some(token) => continuation = Some(token),
                None => break,
            }
        } else {
            break;
        }
    }

    let (current_etags, changed_keys) = s3_plan_changed_keys(&previous_etags, &listed, limits.max_file_bytes);
    let mut objects = Vec::new();
    for (key, _etag, _size) in &listed {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let Some(etag) = current_etags.get(key).cloned() else {
            continue;
        };
        let Some(media_type) = media_type_for_path(Path::new(key)) else {
            continue;
        };
        let object_id = format!("s3-{}", &sha256(format!("{bucket}/{key}").as_bytes())[..32]);
        let canonical_uri = format!("s3://{bucket}/{key}");
        let cached = if changed_keys.contains(key) {
            None
        } else {
            connector_cache_read(&cache_dir, &object_id)
        };
        let bytes = match cached {
            Some(bytes) => bytes,
            None => {
                let canonical_key_uri =
                    format!("/{bucket}/{}", connectors::sigv4_uri_encode(key, false));
                let fetched = s3_signed_get(
                    &endpoint_url,
                    &canonical_key_uri,
                    &[],
                    &access_key,
                    &secret_key,
                    region,
                    false,
                    limits,
                    cancel,
                )
                .await?;
                if fetched.len() as u64 > limits.max_file_bytes {
                    continue;
                }
                connector_cache_write(&cache_dir, &object_id, &fetched)?;
                fetched
            }
        };
        objects.push(source_object_from_bytes(
            &source.id,
            &object_id,
            canonical_uri,
            media_type.to_string(),
            bytes,
            Some(etag),
            None,
        ));
    }
    let cursor = serde_json::to_string(&current_etags).map_err(|error| error.to_string())?;
    Ok((objects, Some(cursor)))
}

// --- NotionPages ----------------------------------------------------------------

async fn notion_get_children(
    block_id: &str,
    token: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<Value>, String> {
    let mut results = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let mut url = format!("https://api.notion.com/v1/blocks/{block_id}/children?page_size=100");
        if let Some(token) = &cursor {
            url.push_str(&format!("&start_cursor={}", percent_encode_query(token)));
        }
        let headers = vec![
            ("authorization", format!("Bearer {token}")),
            ("notion-version", "2022-06-28".to_string()),
        ];
        let (bytes, _headers) =
            fetch_connector_bytes(reqwest::Method::GET, &url, "https://api.notion.com", false, &headers, limits, cancel)
                .await?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Notion returned invalid JSON: {error}"))?;
        if let Some(error) = json.get("message").and_then(Value::as_str) {
            if json.get("object").and_then(Value::as_str) == Some("error") {
                return Err(format!("Notion rejected the request: {error}"));
            }
        }
        if let Some(array) = json.get("results").and_then(Value::as_array) {
            results.extend(array.iter().cloned());
        }
        if json.get("has_more").and_then(Value::as_bool) == Some(true) {
            cursor = json.get("next_cursor").and_then(Value::as_str).map(str::to_string);
            if cursor.is_none() {
                break;
            }
        } else {
            break;
        }
        if results.len() > limits.max_objects_per_source.saturating_mul(5) {
            break;
        }
    }
    Ok(results)
}

fn notion_block_plain_text(block: &Value) -> Option<String> {
    let block_type = block.get("type").and_then(Value::as_str)?;
    let payload = block.get(block_type)?;
    let rich_text = payload.get("rich_text").and_then(Value::as_array)?;
    let combined = rich_text
        .iter()
        .filter_map(|segment| segment.get("plain_text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if combined.trim().is_empty() {
        None
    } else {
        Some(combined)
    }
}

/// BFS from `root_id` collecting every descendant `child_page` block —
/// Notion has no "list pages under this page" endpoint, so this walks
/// `blocks/{id}/children` (paginated) and stops descending once it hits a
/// `child_page` (that page's own content is fetched separately, only if it
/// actually changed — see [`collect_notion_pages`]).
async fn notion_discover_child_pages(
    root_id: &str,
    token: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<(String, String, u64)>, String> {
    let mut found = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back((root_id.to_string(), 0usize));
    let mut visited = BTreeSet::new();
    while let Some((block_id, depth)) = queue.pop_front() {
        if !visited.insert(block_id.clone()) {
            continue;
        }
        if depth > 6 || found.len() >= limits.max_objects_per_source {
            continue;
        }
        let children = notion_get_children(&block_id, token, limits, cancel).await?;
        for child in children {
            let Some(child_id) = child.get("id").and_then(Value::as_str) else {
                continue;
            };
            let block_type = child.get("type").and_then(Value::as_str).unwrap_or_default();
            let last_edited = child
                .get("last_edited_time")
                .and_then(Value::as_str)
                .and_then(parse_iso8601_ms)
                .unwrap_or(0);
            if block_type == "child_page" {
                let title = child
                    .get("child_page")
                    .and_then(|page| page.get("title"))
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled")
                    .to_string();
                found.push((child_id.to_string(), title, last_edited));
            } else if child.get("has_children").and_then(Value::as_bool) == Some(true) {
                queue.push_back((child_id.to_string(), depth + 1));
            }
        }
    }
    Ok(found)
}

async fn notion_extract_page_text(
    page_id: &str,
    title: &str,
    token: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let mut text = format!("# {title}\n\n");
    let mut queue = VecDeque::new();
    queue.push_back((page_id.to_string(), 0usize));
    let mut visited = BTreeSet::new();
    let mut node_budget = 4_000usize;
    while let Some((block_id, depth)) = queue.pop_front() {
        if !visited.insert(block_id.clone()) {
            continue;
        }
        if depth > 8 || node_budget == 0 {
            continue;
        }
        let children = notion_get_children(&block_id, token, limits, cancel).await?;
        for child in children {
            node_budget = node_budget.saturating_sub(1);
            if let Some(line) = notion_block_plain_text(&child) {
                text.push_str(&line);
                text.push('\n');
            }
            let child_type = child.get("type").and_then(Value::as_str).unwrap_or_default();
            if child_type != "child_page" && child.get("has_children").and_then(Value::as_bool) == Some(true) {
                if let Some(id) = child.get("id").and_then(Value::as_str) {
                    queue.push_back((id.to_string(), depth + 1));
                }
            }
        }
    }
    Ok(text)
}

/// Pure incremental-diff planner for [`collect_notion_pages`]: given the
/// previous refresh's per-page `last_edited_time` (as Unix ms, keyed by this
/// source's stable `notion-<id>` object id) and this refresh's freshly
/// discovered `(page_id, title, last_edited_ms)` triples, returns the set of
/// object ids whose `last_edited_time` has moved (or are new) — every other
/// discovered page is expected to already be sitting in the local connector
/// cache.
fn notion_plan_changed_pages(
    previous_by_id: &HashMap<String, u64>,
    discovered: &[(String, String, u64)],
) -> BTreeSet<String> {
    discovered
        .iter()
        .map(|(page_id, _title, last_edited_ms)| {
            let object_id = format!("notion-{}", page_id.replace('-', ""));
            (object_id, *last_edited_ms)
        })
        .filter(|(object_id, last_edited_ms)| previous_by_id.get(object_id) != Some(last_edited_ms))
        .map(|(object_id, _)| object_id)
        .collect()
}

/// Real incremental Notion sync: `source.cursor` is the maximum
/// `last_edited_time` (as Unix ms) observed across every discovered page.
/// Notion's Block Children API has no "changed since" filter, so discovery
/// (`notion_discover_child_pages`) always walks the whole subtree, but each
/// page's own `last_edited_time` (returned inline by that same walk, no
/// extra call) is compared against what this source recorded for it last
/// time (`ConnectorObjectState.modified_unix_ms`) — a page whose timestamp
/// hasn't moved is replayed from `connector_cache_read` instead of walking
/// its blocks again.
async fn collect_notion_pages(
    app_data: &Path,
    source: &KnowledgeSource,
    connector_account_id: &str,
    root_id: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    let account = connectors::account_by_id(connector_account_id)?;
    if account.provider != connectors::ConnectorProvider::Notion {
        return Err("The selected connector account is not a Notion account".to_string());
    }
    let token = connectors::credential_for_account(&account)?;
    let cache_dir = connector_cache_dir(app_data, &source.id)?;
    let previous_by_id: HashMap<String, u64> = source
        .objects
        .iter()
        .filter_map(|object| object.modified_unix_ms.map(|ms| (object.object_id.clone(), ms)))
        .collect();

    let pages = notion_discover_child_pages(root_id, &token, limits, cancel).await?;
    let changed_ids = notion_plan_changed_pages(&previous_by_id, &pages);
    let mut objects = Vec::new();
    let mut max_edited: u64 = 0;
    for (page_id, title, last_edited_ms) in pages {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        max_edited = max_edited.max(last_edited_ms);
        let object_id = format!("notion-{}", page_id.replace('-', ""));
        let canonical_uri = format!("https://notion.so/{}", page_id.replace('-', ""));
        let cached = if changed_ids.contains(&object_id) {
            None
        } else {
            connector_cache_read(&cache_dir, &object_id)
        };
        let bytes = match cached {
            Some(bytes) => bytes,
            None => {
                let text = notion_extract_page_text(&page_id, &title, &token, limits, cancel).await?;
                let bytes = text.into_bytes();
                if bytes.len() as u64 > limits.max_file_bytes {
                    continue;
                }
                connector_cache_write(&cache_dir, &object_id, &bytes)?;
                bytes
            }
        };
        objects.push(source_object_from_bytes(
            &source.id,
            &object_id,
            canonical_uri,
            "text/plain".to_string(),
            bytes,
            None,
            Some(last_edited_ms),
        ));
    }
    Ok((objects, Some(max_edited.to_string())))
}

// --- SlackChannels --------------------------------------------------------------

/// Pure per-page transcript builder for [`slack_fetch_new_messages`]: given
/// one `conversations.history` page's raw `messages` array (Slack returns
/// newest-first — reversed here so appended lines stay chronological) and
/// the `oldest` boundary this refresh started from, returns the new
/// transcript lines plus the latest message timestamp seen. The message
/// exactly at the `oldest` boundary is dropped (Slack's `oldest` is
/// inclusive, and that message was already appended on a previous refresh).
fn slack_parse_messages_page(messages: &[Value], oldest: Option<&str>) -> (Vec<String>, Option<String>) {
    let mut lines = Vec::new();
    let mut latest_ts: Option<String> = None;
    for message in messages.iter().rev() {
        let Some(ts) = message.get("ts").and_then(Value::as_str) else {
            continue;
        };
        if oldest == Some(ts) {
            continue;
        }
        let text = message.get("text").and_then(Value::as_str).unwrap_or("");
        let user = message.get("user").and_then(Value::as_str).unwrap_or("unknown");
        lines.push(format!("[{ts}] {user}: {text}"));
        latest_ts = Some(match latest_ts {
            Some(previous) if previous.as_str() > ts => previous,
            _ => ts.to_string(),
        });
    }
    (lines, latest_ts)
}

/// One page of `conversations.history`, oldest-first within the page
/// (Slack returns newest-first; reversed here so appends stay chronological)
/// and with the message exactly at the `oldest` boundary dropped (Slack's
/// `oldest` is inclusive, and that message was already appended last time).
async fn slack_fetch_new_messages(
    channel_id: &str,
    oldest: Option<&str>,
    token: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<String>, Option<String>), String> {
    let mut lines = Vec::new();
    let mut latest_ts: Option<String> = None;
    let mut cursor: Option<String> = None;
    loop {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let mut url = format!(
            "https://slack.com/api/conversations.history?channel={}&limit=200",
            percent_encode_query(channel_id)
        );
        if let Some(oldest) = oldest {
            url.push_str(&format!("&oldest={}", percent_encode_query(oldest)));
        }
        if let Some(token) = &cursor {
            url.push_str(&format!("&cursor={}", percent_encode_query(token)));
        }
        let headers = vec![("authorization", format!("Bearer {token}"))];
        let (bytes, _headers) =
            fetch_connector_bytes(reqwest::Method::GET, &url, "https://slack.com", false, &headers, limits, cancel)
                .await?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Slack returned invalid JSON: {error}"))?;
        if json.get("ok").and_then(Value::as_bool) != Some(true) {
            let error = json.get("error").and_then(Value::as_str).unwrap_or("unknown_error");
            return Err(format!("Slack rejected the request: {error}"));
        }
        let messages = json
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let (page_lines, page_latest) = slack_parse_messages_page(&messages, oldest);
        lines.extend(page_lines);
        latest_ts = match (latest_ts, page_latest) {
            (Some(previous), Some(candidate)) if previous.as_str() >= candidate.as_str() => {
                Some(previous)
            }
            (_, Some(candidate)) => Some(candidate),
            (previous, None) => previous,
        };
        if json.get("has_more").and_then(Value::as_bool) == Some(true) {
            let next = json
                .get("response_metadata")
                .and_then(|metadata| metadata.get("next_cursor"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        } else {
            break;
        }
        if lines.len() > limits.max_objects_per_source.saturating_mul(20) {
            break;
        }
    }
    Ok((lines, latest_ts))
}

/// Real incremental Slack sync: `source.cursor` is a JSON
/// `{ channel_id: last_ts }` map — each refresh asks `conversations.history`
/// for messages strictly after that channel's own last-seen timestamp
/// (Slack's own `oldest` cursor semantics), appending only the new messages
/// to this source's cached transcript instead of re-fetching channel
/// history from the beginning every time.
async fn collect_slack_channels(
    app_data: &Path,
    source: &KnowledgeSource,
    connector_account_id: &str,
    channel_ids: &[String],
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    let account = connectors::account_by_id(connector_account_id)?;
    if account.provider != connectors::ConnectorProvider::Slack {
        return Err("The selected connector account is not a Slack account".to_string());
    }
    let token = connectors::credential_for_account(&account)?;
    let cache_dir = connector_cache_dir(app_data, &source.id)?;
    let previous_cursors: HashMap<String, String> = source
        .cursor
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let mut next_cursors = previous_cursors.clone();
    let mut objects = Vec::new();
    for channel_id in channel_ids {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let object_id = format!("slack-{}", channel_id.to_ascii_lowercase());
        let oldest = previous_cursors.get(channel_id).cloned();
        let mut transcript = connector_cache_read(&cache_dir, &object_id)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let (new_lines, latest_ts) =
            slack_fetch_new_messages(channel_id, oldest.as_deref(), &token, limits, cancel).await?;
        if !new_lines.is_empty() {
            if !transcript.is_empty() {
                transcript.push('\n');
            }
            transcript.push_str(&new_lines.join("\n"));
            if transcript.len() as u64 > limits.max_file_bytes {
                let keep_from = transcript.len() - limits.max_file_bytes as usize;
                let mut boundary = keep_from;
                while boundary < transcript.len() && !transcript.is_char_boundary(boundary) {
                    boundary += 1;
                }
                transcript = transcript[boundary..].to_string();
            }
            connector_cache_write(&cache_dir, &object_id, transcript.as_bytes())?;
        }
        if let Some(ts) = latest_ts {
            next_cursors.insert(channel_id.clone(), ts);
        }
        if transcript.is_empty() {
            continue;
        }
        let canonical_uri = format!("https://slack.com/archives/{channel_id}");
        objects.push(source_object_from_bytes(
            &source.id,
            &object_id,
            canonical_uri,
            "text/plain".to_string(),
            transcript.into_bytes(),
            None,
            None,
        ));
    }
    let cursor = serde_json::to_string(&next_cursors).map_err(|error| error.to_string())?;
    Ok((objects, Some(cursor)))
}

// --- JiraProject ----------------------------------------------------------------

fn jira_adf_collect(node: &Value, out: &mut String) {
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        out.push_str(text);
    }
    if let Some(content) = node.get("content").and_then(Value::as_array) {
        for child in content {
            jira_adf_collect(child, out);
            out.push(' ');
        }
    }
}

/// Flattens a Jira Cloud v3 Atlassian Document Format `description` field
/// into plain text — good enough for indexing/search, not a full ADF
/// renderer.
fn jira_adf_plain_text(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let mut out = String::new();
    jira_adf_collect(value, &mut out);
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `yyyy-MM-ddTHH:mm:ss.SSS±ZZZZ` → `yyyy-MM-dd HH:mm`, the bound format
/// Jira's JQL `updated >= "..."` clause accepts.
fn jira_jql_timestamp(raw: &str) -> String {
    raw.replacen('T', " ", 1).chars().take(16).collect()
}

/// Real incremental Jira sync: `source.cursor` is a JQL
/// `updated >= "<cursor>"` bound (the maximum `updated` timestamp observed
/// last time) — each refresh's JQL only asks for issues updated since then,
/// so unchanged issues never come back down the wire at all. Every issue
/// this source has ever seen (tracked via its cached object, keyed by issue
/// key) that this sweep did NOT return is still included in the result by
/// replaying its cache, so a stale/deleted-from-JQL issue's chunks aren't
/// spuriously dropped just because it wasn't touched recently.
async fn collect_jira_project(
    app_data: &Path,
    source: &KnowledgeSource,
    connector_account_id: &str,
    project_key: &str,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<SourceObject>, Option<String>), String> {
    let account = connectors::account_by_id(connector_account_id)?;
    if account.provider != connectors::ConnectorProvider::Jira {
        return Err("The selected connector account is not a Jira account".to_string());
    }
    let token = connectors::credential_for_account(&account)?;
    let (site_url, email) = connectors::jira_connection(&account)?;
    let base = Url::parse(&site_url).map_err(|error| format!("Invalid Jira site URL: {error}"))?;
    let origin = origin_of(&base)?;
    let base_str = base.as_str().trim_end_matches('/').to_string();

    let mut jql = format!("project = \"{}\"", project_key.replace('"', ""));
    if let Some(cursor) = source.cursor.as_deref() {
        jql.push_str(&format!(" AND updated >= \"{}\"", cursor.replace('"', "")));
    }
    jql.push_str(" ORDER BY updated ASC");

    let cache_dir = connector_cache_dir(app_data, &source.id)?;
    let credential = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
    let mut objects = Vec::new();
    let mut max_updated_display: Option<String> = None;
    let mut start_at = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let url = format!(
            "{base_str}/rest/api/3/search?jql={}&fields=summary,description,updated,status&startAt={start_at}&maxResults=50",
            percent_encode_query(&jql)
        );
        let headers = vec![
            ("accept", "application/json".to_string()),
            ("authorization", format!("Basic {credential}")),
        ];
        let (bytes, _headers) =
            fetch_connector_bytes(reqwest::Method::GET, &url, &origin, false, &headers, limits, cancel).await?;
        let json: Value = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Jira returned invalid JSON: {error}"))?;
        if let Some(message) = json.get("errorMessages").and_then(Value::as_array) {
            if let Some(first) = message.first().and_then(Value::as_str) {
                return Err(format!("Jira rejected the request: {first}"));
            }
        }
        let issues = json
            .get("issues")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if issues.is_empty() {
            break;
        }
        for issue in &issues {
            let Some(key) = issue.get("key").and_then(Value::as_str) else {
                continue;
            };
            let fields = issue.get("fields").cloned().unwrap_or(Value::Null);
            let summary = fields.get("summary").and_then(Value::as_str).unwrap_or("");
            let updated_raw = fields.get("updated").and_then(Value::as_str).unwrap_or("");
            let updated_ms = parse_iso8601_ms(updated_raw);
            if updated_raw.len() >= 16 {
                let candidate = jira_jql_timestamp(updated_raw);
                max_updated_display = Some(match &max_updated_display {
                    Some(previous) if previous.as_str() >= candidate.as_str() => previous.clone(),
                    _ => candidate,
                });
            }
            let description = jira_adf_plain_text(fields.get("description"));
            let status = fields
                .get("status")
                .and_then(|status| status.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let text = format!("{key}: {summary}\nStatus: {status}\n\n{description}");
            let object_id = format!("jira-{}", key.to_ascii_lowercase());
            let canonical_uri = format!("{base_str}/browse/{key}");
            let bytes = text.into_bytes();
            if bytes.len() as u64 > limits.max_file_bytes {
                continue;
            }
            connector_cache_write(&cache_dir, &object_id, &bytes)?;
            objects.push(source_object_from_bytes(
                &source.id,
                &object_id,
                canonical_uri,
                "text/plain".to_string(),
                bytes,
                None,
                updated_ms,
            ));
        }
        start_at += issues.len() as u32;
        let total = json.get("total").and_then(Value::as_u64).unwrap_or(0) as u32;
        if start_at >= total || issues.len() < 50 || objects.len() > limits.max_objects_per_source {
            break;
        }
    }

    let refreshed_ids: BTreeSet<String> =
        objects.iter().map(|object| object.metadata.object_id.clone()).collect();
    for previous in &source.objects {
        if refreshed_ids.contains(&previous.object_id) {
            continue;
        }
        if let Some(bytes) = connector_cache_read(&cache_dir, &previous.object_id) {
            objects.push(source_object_from_bytes(
                &source.id,
                &previous.object_id,
                previous.canonical_uri.clone(),
                "text/plain".to_string(),
                bytes,
                None,
                previous.modified_unix_ms,
            ));
        }
    }
    let cursor = max_updated_display.or_else(|| source.cursor.clone());
    Ok((objects, cursor))
}

// --- WatchedFolder: filesystem-watcher-driven automatic refresh -----------------

struct WatchedFolderHandle {
    path: PathBuf,
    debounce_ms: u64,
    _watcher: RecommendedWatcher,
}

static WATCHED_FOLDER_HANDLES: OnceLock<Mutex<HashMap<String, WatchedFolderHandle>>> = OnceLock::new();

fn watched_folder_handles() -> &'static Mutex<HashMap<String, WatchedFolderHandle>> {
    WATCHED_FOLDER_HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reconciles the set of active filesystem watchers against every enabled
/// `WatchedFolder` source in the catalog — called once at app startup and
/// again after every catalog mutation (add/update/remove source), so a
/// source's watcher starts/stops in lockstep with the source itself. Stale
/// watchers (for a source that was removed, disabled, or changed connector
/// kind) are dropped, which stops them (`notify`'s watcher unwatches on
/// `Drop`); missing ones are started. Best-effort throughout: a watcher that
/// fails to start (e.g. the path no longer exists) simply means that one
/// source falls back to manual refresh, never a hard error surfaced to the
/// caller — this runs on background/setup paths that have no UI to show one
/// to anyway.
pub fn sync_watched_folder_watchers(app: &AppHandle) {
    let Ok(app_data) = app.path().app_data_dir() else {
        return;
    };
    let Ok(root) = data_root_at(&app_data) else {
        return;
    };
    let Ok(catalog) = load_catalog(&root) else {
        return;
    };
    let mut desired = HashMap::<String, (PathBuf, u64, String)>::new();
    for source in &catalog.sources {
        if !source.enabled {
            continue;
        }
        if let ConnectorConfig::WatchedFolder { path, debounce_ms } = &source.connector {
            desired.insert(source.id.clone(), (PathBuf::from(path), *debounce_ms, source.stack_id.clone()));
        }
    }
    let Ok(mut handles) = watched_folder_handles().lock() else {
        return;
    };
    // Retain a handle only if the source is still an enabled WatchedFolder
    // *and* its watched path/debounce still match the catalog — otherwise an
    // edited source keeps watching its stale, previous path indefinitely.
    handles.retain(|source_id, handle| {
        desired
            .get(source_id)
            .is_some_and(|(path, debounce_ms, _)| handle.path == *path && handle.debounce_ms == *debounce_ms)
    });
    for (source_id, (path, debounce_ms, stack_id)) in desired {
        if handles.contains_key(&source_id) {
            continue;
        }
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            if event.is_ok() {
                let _ = tx.send(());
            }
        });
        let Ok(mut watcher) = watcher else {
            continue;
        };
        if watcher.watch(&path, RecursiveMode::Recursive).is_err() {
            continue;
        }
        let app_for_refresh = app.clone();
        thread::spawn(move || {
            while rx.recv().is_ok() {
                while rx.recv_timeout(Duration::from_millis(debounce_ms)).is_ok() {}
                let app = app_for_refresh.clone();
                let stack_id = stack_id.clone();
                tauri::async_runtime::spawn(async move {
                    let Ok(app_data) = app.path().app_data_dir() else {
                        return;
                    };
                    let _ = knowledge_v2_refresh_headless(&app_data, &stack_id).await;
                });
            }
        });
        handles.insert(source_id, WatchedFolderHandle { path, debounce_ms, _watcher: watcher });
    }
}

fn allowed_extensions() -> Vec<&'static str> {
    vec![
        "txt", "md", "markdown", "html", "htm", "pdf", "docx", "xlsx", "pptx", "png", "jpg",
        "jpeg", "tif", "tiff", "webp", "rs", "ts", "tsx", "js", "jsx", "py", "go", "java", "kt",
        "swift", "c", "h", "cpp", "hpp", "cs", "rb", "php", "sh", "zsh", "fish", "sql", "toml",
        "yaml", "yml", "json", "xml", "css", "scss", "less", "vue", "svelte",
    ]
}

fn collect_local_file(
    source: &KnowledgeSource,
    path: &Path,
    limits: &PipelineLimits,
) -> Result<Vec<SourceObject>, String> {
    let root = path
        .parent()
        .ok_or_else(|| "Local file has no parent directory".to_string())?;
    let policy =
        crate::knowledge_pipeline::LocalSourcePolicy::new([root], allowed_extensions(), false, 64)
            .map_err(|error| error.to_string())?;
    let file = policy
        .validate_file(path, limits)
        .map_err(|error| error.to_string())?;
    Ok(vec![read_validated_file(source, &file)?])
}

fn collect_local_folder(
    source: &KnowledgeSource,
    path: &Path,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<SourceObject>, String> {
    let policy =
        crate::knowledge_pipeline::LocalSourcePolicy::new([path], allowed_extensions(), false, 64)
            .map_err(|error| error.to_string())?;
    let files = policy
        .enumerate_folder(path, limits, cancel)
        .map_err(|error| error.to_string())?;
    files
        .iter()
        .map(|file| read_validated_file(source, file))
        .collect()
}

fn read_validated_file(
    source: &KnowledgeSource,
    file: &crate::knowledge_pipeline::ValidatedFile,
) -> Result<SourceObject, String> {
    let media_type = media_type_for_path(&file.canonical_path)
        .ok_or_else(|| format!("Unsupported file: {}", file.canonical_path.display()))?;
    let bytes = fs::read(&file.canonical_path)
        .map_err(|error| format!("Failed to read {}: {error}", file.canonical_path.display()))?;
    let metadata = fs::metadata(&file.canonical_path).map_err(|error| {
        format!(
            "Failed to inspect {}: {error}",
            file.canonical_path.display()
        )
    })?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64);
    let uri = Url::from_file_path(&file.canonical_path)
        .map_err(|()| "Failed to create file URI".to_string())?
        .to_string();
    let object_id = format!("file-{}", &sha256(uri.as_bytes())[..32]);
    Ok(source_object_from_bytes(
        &source.id,
        &object_id,
        uri,
        media_type.to_string(),
        bytes,
        None,
        modified,
    ))
}

#[derive(Debug)]
struct FetchedBody {
    final_url: String,
    media_type: String,
    bytes: Vec<u8>,
    etag: Option<String>,
    modified: Option<String>,
    resolved_addresses: Vec<IpAddr>,
}

async fn resolve_url(url: &Url) -> Result<Vec<IpAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| format!("DNS resolution failed for {host}: {error}"))?
        .map(|address| address.ip())
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(format!("DNS returned no addresses for {host}"));
    }
    Ok(addresses)
}

async fn fetch_http(
    initial: &str,
    allowed_origin: &str,
    allow_loopback: bool,
    auth: Option<(&str, &str)>,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<FetchedBody, String> {
    let policy = UrlSourcePolicy::new([allowed_origin], allow_loopback, false)
        .map_err(|error| error.to_string())?;
    let mut current = initial.to_string();
    for redirect in 0..=MAX_REDIRECTS {
        if cancel.is_cancelled() {
            return Err(PipelineError::Cancelled.to_string());
        }
        let parsed = Url::parse(&current).map_err(|error| error.to_string())?;
        let addresses = resolve_url(&parsed).await?;
        policy
            .validate(&current, &addresses, limits)
            .map_err(|error| error.to_string())?;
        let host = parsed
            .host_str()
            .ok_or_else(|| "URL has no host".to_string())?;
        let port = parsed
            .port_or_known_default()
            .ok_or_else(|| "URL has no port".to_string())?;
        let socket = SocketAddr::new(addresses[0], port);
        let client = pinned_http_client(host, socket)?;
        let mut request = client.get(parsed.clone());
        if let Some((username, password)) = auth {
            request = request.basic_auth(username, Some(password));
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("Source request failed: {error}"))?;
        if response.status().is_redirection() {
            if redirect == MAX_REDIRECTS {
                return Err("Source exceeded the redirect limit".to_string());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| "Redirect response has no valid Location".to_string())?;
            current = parsed
                .join(location)
                .map_err(|error| format!("Invalid redirect URL: {error}"))?
                .to_string();
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("Source returned HTTP {}", response.status()));
        }
        if let Some(length) = response.content_length() {
            if length > limits.max_file_bytes.min(MAX_HTTP_BYTES as u64) {
                return Err("Source response exceeds the byte limit".to_string());
            }
        }
        let headers = response.headers().clone();
        let media_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("text/html")
            .split(';')
            .next()
            .unwrap_or("text/html")
            .trim()
            .to_ascii_lowercase();
        let etag = headers
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let modified = headers
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            if cancel.is_cancelled() {
                return Err(PipelineError::Cancelled.to_string());
            }
            let chunk = chunk.map_err(|error| format!("Source stream failed: {error}"))?;
            if bytes.len().saturating_add(chunk.len())
                > limits.max_file_bytes.min(MAX_HTTP_BYTES as u64) as usize
            {
                return Err("Source response exceeds the byte limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        return Ok(FetchedBody {
            final_url: current,
            media_type,
            bytes,
            etag,
            modified,
            resolved_addresses: addresses,
        });
    }
    unreachable!()
}

async fn fetch_validated(
    source: &KnowledgeSource,
    url: &str,
    allowed_origin: &str,
    allow_loopback: bool,
    auth: Option<(&str, &str)>,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<SourceObject, String> {
    let fetched = fetch_http(url, allowed_origin, allow_loopback, auth, limits, cancel).await?;
    let object_id = format!("url-{}", &sha256(fetched.final_url.as_bytes())[..32]);
    let mut object = source_object_from_bytes(
        &source.id,
        &object_id,
        fetched.final_url,
        fetched.media_type,
        fetched.bytes,
        fetched.etag,
        None,
    );
    object.metadata.resolved_addresses = fetched.resolved_addresses;
    object.metadata.modified_unix_ms = fetched.modified.as_deref().and_then(parse_http_modified_ms);
    Ok(object)
}

async fn robots_disallow(
    base: &Url,
    allowed_origin: &str,
    allow_loopback: bool,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<String>, String> {
    let robots_url = base
        .join("/robots.txt")
        .map_err(|error| error.to_string())?
        .to_string();
    let fetched = match fetch_http(
        &robots_url,
        allowed_origin,
        allow_loopback,
        None,
        limits,
        cancel,
    )
    .await
    {
        Ok(body) => body,
        Err(_) => return Ok(Vec::new()),
    };
    let text = String::from_utf8_lossy(&fetched.bytes);
    let mut applies = false;
    let mut disallowed = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(agent) = line
            .strip_prefix("User-agent:")
            .or_else(|| line.strip_prefix("user-agent:"))
        {
            applies = agent.trim() == "*";
        } else if applies {
            if let Some(path) = line
                .strip_prefix("Disallow:")
                .or_else(|| line.strip_prefix("disallow:"))
            {
                let path = path.trim();
                if !path.is_empty() {
                    disallowed.push(path.to_string());
                }
            }
        }
    }
    Ok(disallowed)
}

fn links_from_html(base: &Url, bytes: &[u8], allowed_origin: &str) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let html = Html::parse_document(&text);
    let selector = Selector::parse("a[href]").expect("static selector");
    let mut links = html
        .select(&selector)
        .filter_map(|element| element.value().attr("href"))
        .filter_map(|href| base.join(href).ok())
        .filter(|url| origin_of(url).ok().as_deref() == Some(allowed_origin))
        .map(|mut url| {
            url.set_fragment(None);
            url.to_string()
        })
        .collect::<Vec<_>>();
    links.sort();
    links.dedup();
    links
}

#[allow(clippy::too_many_arguments)]
async fn crawl_url(
    source: &KnowledgeSource,
    start_url: &str,
    allowed_origin: &str,
    max_depth: usize,
    max_pages: usize,
    obey_robots: bool,
    allow_loopback: bool,
    seed_urls: Option<Vec<String>>,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<SourceObject>, String> {
    let start = Url::parse(start_url).map_err(|error| error.to_string())?;
    let disallowed = if obey_robots {
        robots_disallow(&start, allowed_origin, allow_loopback, limits, cancel).await?
    } else {
        Vec::new()
    };
    let mut queue = seed_urls
        .unwrap_or_else(|| vec![start_url.to_string()])
        .into_iter()
        .map(|url| (url, 0_usize))
        .collect::<Vec<_>>();
    let mut visited = BTreeSet::new();
    let mut objects = Vec::new();
    while let Some((url, depth)) = queue.first().cloned() {
        queue.remove(0);
        if objects.len() >= max_pages || cancel.is_cancelled() {
            break;
        }
        let parsed = Url::parse(&url).map_err(|error| error.to_string())?;
        if !visited.insert(url.clone())
            || disallowed
                .iter()
                .any(|prefix| parsed.path().starts_with(prefix))
        {
            continue;
        }
        let object = fetch_validated(
            source,
            &url,
            allowed_origin,
            allow_loopback,
            None,
            limits,
            cancel,
        )
        .await?;
        if depth < max_depth && object.metadata.media_type == "text/html" {
            for link in links_from_html(&parsed, &object.bytes, allowed_origin) {
                if !visited.contains(&link) && !queue.iter().any(|(queued, _)| queued == &link) {
                    queue.push((link, depth + 1));
                }
            }
        }
        objects.push(object);
    }
    if cancel.is_cancelled() {
        return Err(PipelineError::Cancelled.to_string());
    }
    Ok(objects)
}

async fn collect_sitemap(
    source: &KnowledgeSource,
    sitemap_url: &str,
    allowed_origin: &str,
    max_pages: usize,
    obey_robots: bool,
    allow_loopback: bool,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<SourceObject>, String> {
    let sitemap = fetch_http(
        sitemap_url,
        allowed_origin,
        allow_loopback,
        None,
        limits,
        cancel,
    )
    .await?;
    let mut reader = quick_xml::Reader::from_reader(sitemap.bytes.as_slice());
    reader.config_mut().trim_text(true);
    let mut in_loc = false;
    let mut urls = Vec::new();
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(start))
                if start.name().as_ref().ends_with(b"loc") =>
            {
                in_loc = true
            }
            Ok(quick_xml::events::Event::Text(text)) if in_loc => {
                let url = text
                    .decode()
                    .map_err(|error| error.to_string())?
                    .to_string();
                if Url::parse(&url)
                    .ok()
                    .and_then(|url| origin_of(&url).ok())
                    .as_deref()
                    == Some(allowed_origin)
                {
                    urls.push(url);
                }
            }
            Ok(quick_xml::events::Event::End(end)) if end.name().as_ref().ends_with(b"loc") => {
                in_loc = false
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(error) => return Err(format!("Invalid sitemap XML: {error}")),
            _ => {}
        }
        if urls.len() >= max_pages {
            break;
        }
    }
    urls.sort();
    urls.dedup();
    crawl_url(
        source,
        sitemap_url,
        allowed_origin,
        0,
        max_pages,
        obey_robots,
        allow_loopback,
        Some(urls),
        limits,
        cancel,
    )
    .await
}

fn collect_selected_chats(
    app_data: &Path,
    source: &KnowledgeSource,
    session_ids: &[String],
    limits: &PipelineLimits,
) -> Result<Vec<SourceObject>, String> {
    let path = app_data.join("chat_sessions.json");
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read conversation profile: {error}"))?;
    if bytes.len() as u64 > limits.max_total_bytes {
        return Err("Conversation profile exceeds the Knowledge byte limit".to_string());
    }
    let root: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse conversation profile: {error}"))?;
    let sessions = root
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .or_else(|| root.as_array())
        .ok_or_else(|| "Conversation profile has no sessions array".to_string())?;
    let selected = session_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut objects = Vec::new();
    for session in sessions {
        let Some(id) = session.get("id").and_then(serde_json::Value::as_str) else {
            continue;
        };
        if !selected.contains(id) {
            continue;
        }
        let title = session
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Conversation");
        let mut text = format!("# {title}\n\n");
        if let Some(messages) = session
            .get("messages")
            .and_then(serde_json::Value::as_array)
        {
            for message in messages {
                let role = message
                    .get("role")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("message");
                let content = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                if !content.trim().is_empty() {
                    text.push_str(&format!("## {role}\n\n{content}\n\n"));
                }
            }
        }
        if text.len() as u64 > limits.max_file_bytes {
            return Err(format!(
                "Conversation {id} exceeds the per-object byte limit"
            ));
        }
        objects.push(source_object_from_bytes(
            &source.id,
            &format!("chat-{}", &sha256(id.as_bytes())[..32]),
            format!("chat://session/{id}"),
            "text/markdown".to_string(),
            text.into_bytes(),
            None,
            session
                .get("updatedAt")
                .or_else(|| session.get("updated_at"))
                .and_then(serde_json::Value::as_u64),
        ));
    }
    Ok(objects)
}

#[derive(Debug)]
struct LocalOverlapReranker;

impl Reranker for LocalOverlapReranker {
    fn reranker_id(&self) -> &str {
        "local.token-overlap.v1"
    }

    fn rerank(
        &self,
        query: &str,
        inputs: &[RerankInput],
        cancel: &CancellationToken,
    ) -> crate::knowledge_pipeline::PipelineResult<Vec<RerankScore>> {
        let query_tokens = tokens(query);
        let mut scores = Vec::with_capacity(inputs.len());
        for input in inputs {
            if cancel.is_cancelled() {
                return Err(PipelineError::Cancelled);
            }
            let document_tokens = tokens(&input.text);
            let overlap = query_tokens.intersection(&document_tokens).count() as i64;
            let coverage = if query_tokens.is_empty() {
                0
            } else {
                overlap.saturating_mul(1_000_000) / query_tokens.len() as i64
            };
            scores.push(RerankScore {
                chunk_id: input.chunk_id.clone(),
                score_micros: coverage.saturating_add((input.fused_score_units / 10_000) as i64),
            });
        }
        Ok(scores)
    }
}

/// True when a stack has a verified active v2 generation. Used by the
/// existing `search_docs` agent tool so an attached stack can migrate to the
/// hybrid index without changing its public tool schema.
pub fn has_active_generation(app: &AppHandle, stack_id: &str) -> Result<bool, String> {
    validate_id("stack id", stack_id)?;
    let store =
        GenerationStore::new(data_root(app)?.join("indexes")).map_err(|error| error.to_string())?;
    store
        .active(stack_id)
        .map(|generation| generation.is_some())
        .map_err(|error| error.to_string())
}

/// Queries one stack's active hybrid generation and adapts the location-aware
/// result to the long-standing `search_docs` return shape. `None` means the
/// stack has not migrated yet and the caller should use its vector-only
/// fallback; errors never silently fall back from a corrupt v2 generation.
/// The agent-facing retrieval path.
///
/// `cancel` is the caller's token, not one minted here. An agent's knowledge
/// search runs inside a turn that can be stopped, and a search that ignored the
/// stop would keep a reranker and a vector scan running after the user had
/// already moved on.
pub async fn query_for_agent(
    app: &AppHandle,
    stack: &KnowledgeStack,
    query: &str,
    k: usize,
    cancel: &CancellationToken,
) -> Result<Option<Vec<crate::stacks::StackQueryResult>>, String> {
    let store =
        GenerationStore::new(data_root(app)?.join("indexes")).map_err(|error| error.to_string())?;
    let Some(index) = store
        .open_active_index(&stack.id)
        .map_err(|error| error.to_string())?
    else {
        return Ok(None);
    };
    let vector = crate::stacks::embed_batch(&stack.embedding, &[query.to_string()], true)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "Embedding provider returned no query vector".to_string())?;
    let config = HybridSearchConfig {
        lexical_candidates: k.max(50),
        vector_candidates: k.max(50),
        final_results: k.max(1),
        rerank_candidates: k.max(20),
        ..HybridSearchConfig::default()
    };
    // The reranker and the token both matter for parity with the inspector path
    // (`knowledge_v2_query`): without the reranker, the agent and the
    // "test search" box in the panel returned differently-ordered results for
    // the same query against the same index, and the panel was the one telling
    // the truth about what retrieval does.
    let reranker = LocalOverlapReranker;
    let response = index
        .search(
            query,
            &vector,
            &config,
            &PipelineLimits::default(),
            Some(&reranker),
            cancel,
        )
        .map_err(|error| error.to_string())?;
    Ok(Some(
        response
            .hits
            .into_iter()
            .map(|hit| {
                let text = match low_confidence_notice(&hit.chunk) {
                    Some(notice) => format!("{notice}\n{}", hit.chunk.text),
                    None => hit.chunk.text.clone(),
                };
                crate::stacks::StackQueryResult {
                    stack_id: stack.id.clone(),
                    stack_name: stack.name.clone(),
                    source_path: hit.chunk.citation.canonical_uri.clone(),
                    score: (hit.fused_score_units as f64 / 40_000_000_000_f64).min(1.0) as f32,
                    text,
                    heading: (!hit.chunk.heading_path.is_empty())
                        .then(|| hit.chunk.heading_path.join(" > ")),
                }
            })
            .collect(),
    ))
}

fn tokens(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| token.len() > 1)
        .take(1_000)
        .map(str::to_ascii_lowercase)
        .collect()
}

#[tauri::command]
pub fn knowledge_v2_update_chunking(
    app: AppHandle,
    stack_id: String,
    chunk_chars: usize,
    chunk_overlap: usize,
) -> Result<KnowledgeStack, String> {
    validate_id("stack id", &stack_id)?;
    let stacks_root = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("stacks");
    crate::stacks::update_chunking_impl(&stacks_root, &stack_id, chunk_chars, chunk_overlap)
}

fn low_confidence_notice(chunk: &KnowledgeChunk) -> Option<String> {
    if chunk.is_low_confidence_ocr() {
        let confidence = chunk
            .effective_confidence_micros()
            .map(|value| format!("{:.1}%", f64::from(value) / 10_000.0))
            .unwrap_or_else(|| "unknown confidence".to_string());
        Some(format!(
            "[LOW-CONFIDENCE OCR · {confidence} · verify against the cited source before relying on this text]"
        ))
    } else {
        None
    }
}

fn inspector_context_entry(rank: u32, chunk: &KnowledgeChunk) -> String {
    let confidence_warning = low_confidence_notice(chunk)
        .map(|notice| format!("\n{notice}"))
        .unwrap_or_default();
    format!(
        "[{rank}] {}{}\n{}\n\n",
        chunk.citation.canonical_uri, confidence_warning, chunk.text
    )
}

#[tauri::command]
pub async fn knowledge_v2_query(
    app: AppHandle,
    request: KnowledgeQueryRequest,
) -> Result<KnowledgeInspectorResponse, String> {
    validate_id("stack id", &request.stack_id)?;
    let query_id = request
        .query_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let (cancel, _registration) = register_query_cancellation(&query_id)?;
    let normalized_query = request
        .query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized_query.is_empty() {
        return Err("Query cannot be empty".to_string());
    }
    let root = data_root(&app)?;
    let stacks_root = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("stacks");
    let stack = crate::stacks::list_impl(&stacks_root)?
        .into_iter()
        .find(|stack| stack.id == request.stack_id)
        .ok_or_else(|| "Knowledge stack not found".to_string())?;
    let generation_store =
        GenerationStore::new(root.join("indexes")).map_err(|error| error.to_string())?;
    let index = generation_store
        .open_active_index(&request.stack_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Knowledge 2.0 stack has not been indexed".to_string())?;
    let vector = crate::stacks::embed_batch(&stack.embedding, &[normalized_query.clone()], true)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "Embedding provider returned no query vector".to_string())?;
    if cancel.is_cancelled() {
        return Err("Knowledge query cancelled".to_string());
    }
    let reranker = LocalOverlapReranker;
    let excluded = request
        .excluded_source_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let search = index
        .search_excluding_sources(
            &normalized_query,
            &vector,
            &request.config,
            &PipelineLimits::default(),
            request.rerank.then_some(&reranker as &dyn Reranker),
            &excluded,
            &cancel,
        )
        .map_err(|error| error.to_string())?;
    let mut final_context = String::new();
    let mut estimated_tokens = 0_usize;
    for hit in &search.hits {
        if cancel.is_cancelled() {
            return Err("Knowledge query cancelled".to_string());
        }
        let entry = inspector_context_entry(hit.rank, &hit.chunk);
        let estimate = entry.chars().count().div_ceil(4);
        if estimated_tokens.saturating_add(estimate) > request.token_budget {
            break;
        }
        estimated_tokens += estimate;
        final_context.push_str(&entry);
    }
    Ok(KnowledgeInspectorResponse {
        query_id,
        normalized_query,
        excluded_source_ids: request.excluded_source_ids,
        token_budget: request.token_budget,
        estimated_context_tokens: estimated_tokens,
        final_context,
        search,
    })
}

#[tauri::command]
pub fn knowledge_v2_pii_preview(text: String) -> Result<RedactionPreview, String> {
    if text.len() > 4 * 1024 * 1024 {
        return Err("PII preview text exceeds 4 MiB".to_string());
    }
    SensitiveDataScanner::new()
        .and_then(|scanner| scanner.apply_policy(&text, SensitiveDataMode::ReportOnly))
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn knowledge_ocr_status(app: AppHandle) -> Result<KnowledgeOcrConfig, String> {
    load_ocr_config(&data_root(&app)?)
}

#[tauri::command]
pub fn knowledge_ocr_configure_external(
    app: AppHandle,
    executable_path: String,
    pdf_renderer_path: Option<String>,
    languages: Vec<String>,
    low_confidence_micros: u32,
) -> Result<KnowledgeOcrConfig, String> {
    let executable = validate_regular_executable(&executable_path, "OCR executable")?;
    let bytes =
        fs::read(&executable).map_err(|error| format!("Failed to hash OCR executable: {error}"))?;
    let renderer = pdf_renderer_path
        .filter(|path| !path.trim().is_empty())
        .map(|path| validate_regular_executable(&path, "PDF renderer"))
        .transpose()?;
    let config = KnowledgeOcrConfig {
        enabled: true,
        executable_path: Some(executable.to_string_lossy().into_owned()),
        pdf_renderer_path: renderer.map(|path| path.to_string_lossy().into_owned()),
        asset: Some(OcrAssetMetadata {
            asset_id: "knowledge-ocr-external".to_string(),
            sha256: sha256(&bytes),
            engine: "tesseract".to_string(),
            engine_version: "external".to_string(),
            languages: languages.clone(),
            license: "User-managed installation".to_string(),
            provenance: executable.to_string_lossy().into_owned(),
        }),
        languages,
        low_confidence_micros,
    };
    save_ocr_config(&data_root(&app)?, &config)?;
    Ok(config)
}

#[tauri::command]
pub async fn knowledge_ocr_install(
    app: AppHandle,
    request: OcrInstallRequest,
) -> Result<KnowledgeOcrConfig, String> {
    if request.expected_sha256.len() != 64
        || !request
            .expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || request.size_bytes == 0
        || request.size_bytes > 256 * 1024 * 1024
        || request.version.is_empty()
        || request.version.len() > 80
        || request.license_name.trim().is_empty()
        || request.provenance.trim().is_empty()
    {
        return Err("Invalid OCR install metadata".to_string());
    }
    let parsed = Url::parse(&request.url).map_err(|error| format!("Invalid OCR URL: {error}"))?;
    if parsed.scheme() != "https" {
        return Err("OCR sidecars must be downloaded over HTTPS".to_string());
    }
    let origin = origin_of(&parsed)?;
    let mut limits = PipelineLimits::default();
    limits.max_file_bytes = 256 * 1024 * 1024;
    limits.max_total_bytes = 256 * 1024 * 1024;
    let fetched = fetch_http(
        &request.url,
        &origin,
        false,
        None,
        &limits,
        &CancellationToken::new(),
    )
    .await?;
    if fetched.bytes.len() as u64 != request.size_bytes {
        return Err(format!(
            "OCR download size mismatch: expected {}, received {}",
            request.size_bytes,
            fetched.bytes.len()
        ));
    }
    let root = data_root(&app)?;
    let manager = crate::asset_manager::AssetManager::with_default_quota(root.join("assets"))
        .map_err(|error| error.to_string())?;
    let installed = manager
        .upgrade_reader(
            &crate::asset_manager::AssetInstallRequest {
                asset_id: "knowledge-ocr-tesseract".to_string(),
                kind: crate::asset_manager::AssetKind::Sidecar,
                version: request.version.clone(),
                source: crate::asset_manager::AssetSource {
                    uri: request.url.clone(),
                    revision: Some(request.version.clone()),
                },
                provenance: crate::asset_manager::AssetProvenance {
                    publisher: None,
                    retrieved_at_ms: Some(now_ms()),
                    notes: Some(request.provenance.clone()),
                },
                license: crate::asset_manager::AssetLicense {
                    name: request.license_name.clone(),
                    spdx_id: None,
                    url: request.license_url.clone(),
                    text: None,
                },
                platform: crate::asset_manager::AssetPlatform::current(),
                expected_sha256: request.expected_sha256.clone(),
                size_bytes: request.size_bytes,
            },
            std::io::Cursor::new(fetched.bytes),
        )
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            &installed.version.payload_path,
            fs::Permissions::from_mode(0o700),
        )
        .map_err(|error| format!("Failed to mark OCR sidecar executable: {error}"))?;
    }
    let config = KnowledgeOcrConfig {
        enabled: true,
        executable_path: Some(
            installed
                .version
                .payload_path
                .to_string_lossy()
                .into_owned(),
        ),
        pdf_renderer_path: load_ocr_config(&root)?.pdf_renderer_path,
        asset: Some(OcrAssetMetadata {
            asset_id: "knowledge-ocr-tesseract".to_string(),
            sha256: request.expected_sha256,
            engine: "tesseract".to_string(),
            engine_version: request.version,
            languages: request.languages.clone(),
            license: request.license_name,
            provenance: request.provenance,
        }),
        languages: request.languages,
        low_confidence_micros: 800_000,
    };
    save_ocr_config(&root, &config)?;
    Ok(config)
}

#[tauri::command]
pub fn knowledge_ocr_set_enabled(
    app: AppHandle,
    enabled: bool,
) -> Result<KnowledgeOcrConfig, String> {
    let root = data_root(&app)?;
    let mut config = load_ocr_config(&root)?;
    config.enabled = enabled;
    save_ocr_config(&root, &config)?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temporary_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-knowledge-service-{label}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn catalog_round_trip_is_atomic_and_filters_duplicate_ids() {
        let root = temporary_root("catalog");
        let catalog = KnowledgeCatalog {
            version: CATALOG_VERSION,
            sources: vec![KnowledgeSource {
                id: "source-1".into(),
                stack_id: "stack-1".into(),
                label: "Docs".into(),
                enabled: true,
                connector: ConnectorConfig::SelectedChats {
                    session_ids: vec!["session-1".into()],
                },
                cursor: None,
                checkpoint: None,
                last_refresh_at_ms: None,
                last_error: None,
                objects: Vec::new(),
                retries: Vec::new(),
            }],
        };
        save_catalog(&root, &catalog).unwrap();
        assert_eq!(load_catalog(&root).unwrap(), catalog);
        save_catalog(&root, &catalog).unwrap();
        assert_eq!(load_catalog(&root).unwrap(), catalog);
        assert!(!root.join("catalog.json.tmp").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_duplicate_refresh_registration_preserves_the_running_token() {
        let stack_id = format!("stack-{}", Uuid::new_v4());
        let (running, registration) = register_cancellation(&stack_id).unwrap();
        assert!(register_cancellation(&stack_id).is_err());
        assert!(knowledge_v2_cancel_refresh(stack_id.clone()).unwrap());
        assert!(running.is_cancelled());
        drop(registration);
        assert!(!knowledge_v2_cancel_refresh(stack_id).unwrap());
    }

    #[test]
    fn query_cancellation_is_scoped_to_the_exact_registered_id() {
        let query_id = format!("query-{}", Uuid::new_v4());
        let other_id = format!("query-{}", Uuid::new_v4());
        let (running, registration) = register_query_cancellation(&query_id).unwrap();
        assert!(register_query_cancellation(&query_id).is_err());
        assert!(!knowledge_v2_cancel_query(other_id).unwrap());
        assert!(knowledge_v2_cancel_query(query_id.clone()).unwrap());
        assert!(running.is_cancelled());
        drop(registration);
        assert!(!knowledge_v2_cancel_query(query_id).unwrap());
    }

    #[test]
    fn inspector_context_visibly_marks_low_confidence_ocr() {
        let text = "uncertain source text".to_string();
        let text_sha256 = sha256(text.as_bytes());
        let location = crate::knowledge_pipeline::DocumentLocation::Ocr {
            asset_id: "ocr:fixture".to_string(),
            page: 2,
            bbox: crate::knowledge_pipeline::BoundingBox {
                x: 1.0,
                y: 1.0,
                width: 10.0,
                height: 5.0,
            },
            confidence_micros: 710_000,
        };
        let chunk = KnowledgeChunk {
            chunk_id: sha256(b"chunk"),
            source_id: "source:ocr".to_string(),
            object_id: "object:ocr".to_string(),
            object_content_sha256: sha256(b"object"),
            text_sha256,
            text: text.clone(),
            heading_path: Vec::new(),
            location: location.clone(),
            block_char_start: 0,
            block_char_end: text.len() as u64,
            citation: crate::knowledge_pipeline::Citation {
                citation_id: sha256(b"citation"),
                source_id: "source:ocr".to_string(),
                object_id: "object:ocr".to_string(),
                canonical_uri: "file:///scanned.pdf".to_string(),
                location,
                block_char_start: 0,
                block_char_end: text.len() as u64,
            },
            content_role: crate::knowledge_pipeline::ContentRole::RetrievedData,
            content_type: "ocr_low_confidence".to_string(),
            confidence_micros: Some(710_000),
            low_confidence: true,
        };
        let entry = inspector_context_entry(3, &chunk);
        assert!(entry.contains("LOW-CONFIDENCE OCR"));
        assert!(entry.contains("71.0%"));
        assert!(entry.contains("verify against the cited source"));
        assert!(entry.contains(&text));
    }

    #[test]
    fn cross_process_refresh_lease_is_exclusive_and_recovers_stale_owner() {
        let app_data = temporary_root("refresh-lease");
        let stack_id = "stack-lease";
        let first = acquire_refresh_lease(&app_data, stack_id).unwrap();
        let duplicate = acquire_refresh_lease(&app_data, stack_id).unwrap_err();
        assert!(duplicate.contains("already running"));
        drop(first);

        let second = acquire_refresh_lease(&app_data, stack_id).unwrap();
        drop(second);
        let directory = refresh_lease_directory(&app_data).unwrap();
        let path = directory.join(format!("{stack_id}.lock"));
        let stale = RefreshLeaseRecord {
            version: 1,
            stack_id: stack_id.to_string(),
            owner_pid: u32::MAX,
            acquired_at_ms: now_ms().saturating_sub(REFRESH_LEASE_STALE_MS + 1),
            token: "stale-owner-token".to_string(),
        };
        atomic_write_private(
            &path,
            &serde_json::to_vec(&stale).unwrap(),
            "test refresh lease",
        )
        .unwrap();
        let recovered = acquire_refresh_lease(&app_data, stack_id).unwrap();
        assert_ne!(recovered.token, stale.token);
        drop(recovered);
        assert!(!path.exists());
        fs::remove_dir_all(app_data).unwrap();
    }

    #[tokio::test]
    async fn daemon_background_schedule_is_opt_in_bounded_and_advances_before_reuse() {
        let app_data = temporary_root("background");
        let config = KnowledgeBackgroundRefreshConfig {
            enabled: true,
            interval_minutes: 5,
            stack_ids: Vec::new(),
            last_attempt_ms: None,
            last_success_ms: None,
            next_due_ms: Some(1_000),
            last_error: None,
            consecutive_failures: 0,
        };
        save_background_refresh_config_at(&app_data, &config).unwrap();
        let outcome = run_due_background_refresh(&app_data, 1_000).await.unwrap();
        assert!(outcome.due);
        assert!(outcome.failures.is_empty());
        assert_eq!(outcome.next_due_ms, Some(301_000));

        let repeated = run_due_background_refresh(&app_data, 1_001).await.unwrap();
        assert!(!repeated.due);
        let stored = load_background_refresh_config_at(&app_data).unwrap();
        assert_eq!(stored.last_attempt_ms, Some(1_000));
        assert!(stored.last_success_ms.is_some());
        assert_eq!(stored.consecutive_failures, 0);
        assert!(stored.last_error.is_none());

        let failing = KnowledgeBackgroundRefreshConfig {
            enabled: true,
            interval_minutes: 5,
            stack_ids: vec!["missing-stack".to_string()],
            last_attempt_ms: stored.last_attempt_ms,
            last_success_ms: stored.last_success_ms,
            next_due_ms: Some(302_000),
            last_error: None,
            consecutive_failures: 0,
        };
        save_background_refresh_config_at(&app_data, &failing).unwrap();
        let failed = run_due_background_refresh(&app_data, 302_000)
            .await
            .unwrap();
        assert_eq!(failed.failures.len(), 1);
        let failed_config = load_background_refresh_config_at(&app_data).unwrap();
        assert_eq!(failed_config.consecutive_failures, 1);
        assert!(failed_config
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("Knowledge stack not found")));

        let invalid = KnowledgeBackgroundRefreshConfig {
            interval_minutes: 4,
            ..KnowledgeBackgroundRefreshConfig::default()
        };
        assert!(validate_background_refresh_config(invalid).is_err());
        fs::remove_dir_all(app_data).unwrap();
    }

    #[test]
    fn http_last_modified_is_parsed_as_time_not_an_opaque_hash() {
        assert_eq!(
            parse_http_modified_ms("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777_000)
        );
        assert_eq!(parse_http_modified_ms("not a date"), None);
    }

    #[test]
    fn validates_source_boundaries_and_url_origins() {
        let error = validate_connector(&ConnectorConfig::Url {
            url: "https://example.com/docs".into(),
            allowed_origin: "https://evil.example".into(),
            max_depth: 1,
            max_pages: 5,
            obey_robots: true,
            allow_loopback: false,
        })
        .unwrap_err();
        assert!(error.contains("allowed origin"));
        assert!(validate_connector(&ConnectorConfig::SelectedChats {
            session_ids: vec![],
        })
        .is_err());
    }

    #[test]
    fn local_reranker_is_deterministic_and_cancel_aware() {
        let reranker = LocalOverlapReranker;
        let inputs = vec![
            RerankInput {
                chunk_id: "a".into(),
                text: "alpha beta".into(),
                fused_score_units: 10,
            },
            RerankInput {
                chunk_id: "b".into(),
                text: "gamma".into(),
                fused_score_units: 20,
            },
        ];
        let scores = reranker
            .rerank("alpha", &inputs, &CancellationToken::new())
            .unwrap();
        assert!(scores[0].score_micros > scores[1].score_micros);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            reranker.rerank("alpha", &inputs, &cancel),
            Err(PipelineError::Cancelled)
        ));
    }

    #[test]
    fn pii_preview_never_echoes_secret_in_masked_metadata() {
        let preview = knowledge_v2_pii_preview(
            "api_key = supersecrettoken123456 user@example.com".to_string(),
        )
        .unwrap();
        assert!(!preview.findings.is_empty());
        assert!(preview
            .findings
            .iter()
            .all(|finding| !finding.masked_preview.contains("supersecret")));
    }

    // =========================================================================
    // External Knowledge Sync connectors — dedup/incremental-cursor planners.
    // Every test below is pure/fixture-driven: no live network call, no `gh`
    // process, matching `knowledge_pipeline.rs`'s
    // `url_policy_enforces_origin_dns_and_ssrf_limits` style.
    // =========================================================================

    // --- GitHubRepo ----------------------------------------------------------

    #[test]
    fn github_plan_paths_does_a_full_tree_listing_on_first_refresh() {
        let tree = vec![
            json!({ "type": "blob", "path": "README.md" }),
            json!({ "type": "blob", "path": "docs/guide.md" }),
            json!({ "type": "tree", "path": "docs" }),
            json!({ "type": "blob", "path": "docs/notes.bin" }),
        ];
        let (all_paths, changed_paths) =
            github_plan_paths(BTreeSet::new(), GithubCursorState::FullTree(tree), None);
        assert_eq!(
            all_paths,
            BTreeSet::from(["README.md".to_string(), "docs/guide.md".to_string()])
        );
        assert_eq!(all_paths, changed_paths, "every path is 'changed' on a first sync");
    }

    #[test]
    fn github_plan_paths_only_marks_compare_reported_files_as_changed() {
        let previous = BTreeSet::from([
            "README.md".to_string(),
            "docs/guide.md".to_string(),
            "docs/old.md".to_string(),
        ]);
        let files = vec![
            json!({ "filename": "docs/guide.md", "status": "modified" }),
            json!({ "filename": "docs/old.md", "status": "removed" }),
            json!({ "filename": "docs/new.md", "status": "added" }),
        ];
        let (all_paths, changed_paths) =
            github_plan_paths(previous, GithubCursorState::Compare(files), None);
        assert_eq!(
            all_paths,
            BTreeSet::from([
                "README.md".to_string(),
                "docs/guide.md".to_string(),
                "docs/new.md".to_string(),
            ]),
            "old.md is dropped and new.md is added"
        );
        assert_eq!(
            changed_paths,
            BTreeSet::from(["docs/guide.md".to_string(), "docs/new.md".to_string()]),
            "README.md never appears in the compare diff, so it must NOT be re-fetched"
        );
    }

    #[test]
    fn github_plan_paths_an_unchanged_cursor_re_fetches_nothing() {
        let previous = BTreeSet::from(["README.md".to_string()]);
        let (all_paths, changed_paths) =
            github_plan_paths(previous.clone(), GithubCursorState::Unchanged, None);
        assert_eq!(all_paths, previous);
        assert!(changed_paths.is_empty());
    }

    #[test]
    fn github_plan_paths_a_rename_drops_the_old_path_and_keeps_the_new_one_changed() {
        let previous = BTreeSet::from(["old_name.md".to_string()]);
        let files = vec![
            json!({ "filename": "new_name.md", "status": "renamed", "previous_filename": "old_name.md" }),
        ];
        let (all_paths, changed_paths) =
            github_plan_paths(previous, GithubCursorState::Compare(files), None);
        assert_eq!(all_paths, BTreeSet::from(["new_name.md".to_string()]));
        assert_eq!(changed_paths, BTreeSet::from(["new_name.md".to_string()]));
    }

    #[test]
    fn github_plan_paths_respects_the_configured_path_prefix() {
        let tree = vec![
            json!({ "type": "blob", "path": "docs/guide.md" }),
            json!({ "type": "blob", "path": "src/main.rs" }),
        ];
        let (all_paths, _) =
            github_plan_paths(BTreeSet::new(), GithubCursorState::FullTree(tree), Some("docs"));
        assert_eq!(all_paths, BTreeSet::from(["docs/guide.md".to_string()]));
    }

    // --- S3Bucket --------------------------------------------------------------

    #[test]
    fn s3_plan_changed_keys_only_re_fetches_keys_whose_etag_moved() {
        let previous = HashMap::from([
            ("reports/a.txt".to_string(), "etag-a".to_string()),
            ("reports/b.txt".to_string(), "etag-b".to_string()),
        ]);
        let listed = vec![
            ("reports/a.txt".to_string(), "etag-a".to_string(), 10),
            ("reports/b.txt".to_string(), "etag-b2".to_string(), 20),
            ("reports/c.txt".to_string(), "etag-c".to_string(), 30),
        ];
        let (current, changed) = s3_plan_changed_keys(&previous, &listed, 1_000_000);
        assert_eq!(current.len(), 3);
        assert_eq!(
            changed,
            BTreeSet::from(["reports/b.txt".to_string(), "reports/c.txt".to_string()]),
            "only the moved ETag and the brand-new key should be marked changed"
        );
    }

    #[test]
    fn s3_plan_changed_keys_skips_oversized_and_unsupported_extension_objects() {
        let previous = HashMap::new();
        let listed = vec![
            ("huge.txt".to_string(), "etag-1".to_string(), 1_000),
            ("archive.tar.gz".to_string(), "etag-2".to_string(), 10),
            ("notes.md".to_string(), "etag-3".to_string(), 10),
        ];
        let (current, changed) = s3_plan_changed_keys(&previous, &listed, 100);
        assert_eq!(current.len(), 1, "only notes.md fits the size AND extension filter");
        assert!(current.contains_key("notes.md"));
        assert_eq!(changed, BTreeSet::from(["notes.md".to_string()]));
    }

    // --- NotionPages -------------------------------------------------------------

    #[test]
    fn notion_plan_changed_pages_only_flags_pages_whose_last_edited_time_moved() {
        let previous = HashMap::from([
            ("notion-aaa".to_string(), 1_000u64),
            ("notion-bbb".to_string(), 2_000u64),
        ]);
        let discovered = vec![
            ("aaa".to_string(), "Unchanged Page".to_string(), 1_000u64),
            ("bbb".to_string(), "Edited Page".to_string(), 2_500u64),
            ("ccc".to_string(), "New Page".to_string(), 3_000u64),
        ];
        let changed = notion_plan_changed_pages(&previous, &discovered);
        assert_eq!(
            changed,
            BTreeSet::from(["notion-bbb".to_string(), "notion-ccc".to_string()]),
            "aaa's last_edited_time is identical to last time, so it must be replayed from cache"
        );
    }

    // --- SlackChannels -----------------------------------------------------------

    #[test]
    fn slack_parse_messages_page_reverses_to_chronological_order_and_drops_the_oldest_boundary() {
        let messages = vec![
            json!({ "ts": "3.0", "user": "u1", "text": "third" }),
            json!({ "ts": "2.0", "user": "u1", "text": "second" }),
            json!({ "ts": "1.0", "user": "u1", "text": "first (already synced)" }),
        ];
        let (lines, latest) = slack_parse_messages_page(&messages, Some("1.0"));
        assert_eq!(lines, vec!["[2.0] u1: second".to_string(), "[3.0] u1: third".to_string()]);
        assert_eq!(latest.as_deref(), Some("3.0"));
    }

    #[test]
    fn slack_parse_messages_page_with_no_oldest_boundary_keeps_every_message() {
        let messages = vec![json!({ "ts": "5.0", "user": "u2", "text": "hello" })];
        let (lines, latest) = slack_parse_messages_page(&messages, None);
        assert_eq!(lines, vec!["[5.0] u2: hello".to_string()]);
        assert_eq!(latest.as_deref(), Some("5.0"));
    }

    // --- JiraProject ---------------------------------------------------------------

    #[test]
    fn jira_jql_timestamp_converts_the_iso8601_updated_field_to_the_jql_bound_format() {
        assert_eq!(jira_jql_timestamp("2024-01-15T10:30:00.000+0000"), "2024-01-15 10:30");
    }

    #[test]
    fn jira_adf_plain_text_flattens_nested_content_nodes() {
        let adf = json!({
            "type": "doc",
            "content": [
                { "type": "paragraph", "content": [
                    { "type": "text", "text": "Steps to reproduce:" }
                ] },
                { "type": "bulletList", "content": [
                    { "type": "listItem", "content": [
                        { "type": "paragraph", "content": [ { "type": "text", "text": "Open the app" } ] }
                    ] }
                ] }
            ]
        });
        let text = jira_adf_plain_text(Some(&adf));
        assert!(text.contains("Steps to reproduce:"));
        assert!(text.contains("Open the app"));
    }

    #[test]
    fn jira_adf_plain_text_of_none_is_empty() {
        assert_eq!(jira_adf_plain_text(None), "");
    }

    // --- shared authenticated fetch + S3 signing, against a fixture server ------

    /// Same shape as `connectors.rs`'s `spawn_fixture` (status line/extra
    /// headers/body in, local loopback address out), plus a channel that
    /// hands back the raw request bytes the fixture received — so a test can
    /// assert on the request (headers, query string, ...) from the main test
    /// thread instead of panicking inside the accept thread, where a panic
    /// wouldn't fail the test directly.
    fn spawn_knowledge_fixture(
        status_line: &str,
        extra_headers: &str,
        body: &'static str,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let status_line = status_line.to_string();
        let extra_headers = extra_headers.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let read = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]).to_string();
                let _ = tx.send(request);
                let response = format!(
                    "{status_line}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn fetch_connector_bytes_sends_the_given_headers_and_returns_the_body() {
        let (addr, rx) = spawn_knowledge_fixture("HTTP/1.1 200 OK", "", "{\"ok\":true}");
        let origin = format!("http://{addr}");
        let url = format!("http://{addr}/v1/users/me");
        let (bytes, _headers) = fetch_connector_bytes(
            reqwest::Method::GET,
            &url,
            &origin,
            true,
            &[("authorization", "Bearer fixture-token".to_string())],
            &PipelineLimits::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(bytes, b"{\"ok\":true}");
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            request.to_lowercase().contains("authorization: bearer fixture-token"),
            "{request}"
        );
    }

    #[tokio::test]
    async fn fetch_connector_bytes_blocks_loopback_by_default() {
        let url = "http://127.0.0.1:9/v1/users/me";
        let result = fetch_connector_bytes(
            reqwest::Method::GET,
            url,
            "http://127.0.0.1:9",
            false,
            &[],
            &PipelineLimits::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn fetch_connector_bytes_refuses_a_redirect() {
        let (addr, _rx) = spawn_knowledge_fixture(
            "HTTP/1.1 302 Found",
            "Location: http://example.test/\r\n",
            "",
        );
        let origin = format!("http://{addr}");
        let url = format!("http://{addr}/");
        let result = fetch_connector_bytes(
            reqwest::Method::GET,
            &url,
            &origin,
            true,
            &[],
            &PipelineLimits::default(),
            &CancellationToken::new(),
        )
        .await;
        match result {
            Ok(_) => panic!("expected the redirect to be refused"),
            Err(message) => assert!(message.to_lowercase().contains("redirect"), "{message}"),
        }
    }

    #[tokio::test]
    async fn s3_signed_get_sends_a_sigv4_authorization_header_and_the_exact_canonical_query() {
        let (addr, rx) = spawn_knowledge_fixture(
            "HTTP/1.1 200 OK",
            "",
            "<ListBucketResult></ListBucketResult>",
        );
        let endpoint_url = Url::parse(&format!("http://{addr}")).unwrap();
        let bytes = s3_signed_get(
            &endpoint_url,
            "/my-bucket",
            &[("list-type", "2"), ("prefix", "notes/")],
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            true,
            &PipelineLimits::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("ListBucketResult"));
        let request = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let request_line = request.lines().next().unwrap_or_default();
        assert!(
            request_line.contains("list-type=2") && request_line.contains("prefix=notes%2F"),
            "request line should carry the exact signed canonical query: {request_line}"
        );
        assert!(
            request.to_lowercase().contains("authorization: aws4-hmac-sha256"),
            "request should carry a SigV4 Authorization header: {request}"
        );
    }

    // --- connector cache -----------------------------------------------------------

    #[test]
    fn connector_cache_round_trips_bytes_and_misses_cleanly_when_absent() {
        let root = temporary_root("connector-cache");
        let cache_dir = connector_cache_dir(&root, "source-1").unwrap();
        assert!(connector_cache_read(&cache_dir, "object-1").is_none());
        connector_cache_write(&cache_dir, "object-1", b"hello world").unwrap();
        assert_eq!(
            connector_cache_read(&cache_dir, "object-1").as_deref(),
            Some(b"hello world".as_slice())
        );
        fs::remove_dir_all(root).unwrap();
    }

    // --- validate_connector: new connector kinds -------------------------------

    #[test]
    fn validate_connector_rejects_a_watched_folder_debounce_outside_bounds() {
        let root = temporary_root("watched-folder");
        let error = validate_connector(&ConnectorConfig::WatchedFolder {
            path: root.to_string_lossy().to_string(),
            debounce_ms: 50,
        })
        .unwrap_err();
        assert!(error.contains("debounce"), "{error}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_connector_accepts_a_watched_folder_within_debounce_bounds() {
        let root = temporary_root("watched-folder-ok");
        validate_connector(&ConnectorConfig::WatchedFolder {
            path: root.to_string_lossy().to_string(),
            debounce_ms: 1_000,
        })
        .unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_connector_rejects_an_unknown_connector_account_id() {
        let error = validate_connector(&ConnectorConfig::GitHubRepo {
            owner: "acme".to_string(),
            repo: "widgets".to_string(),
            git_ref: None,
            path_prefix: None,
            connector_account_id: "does-not-exist".to_string(),
        })
        .unwrap_err();
        assert!(error.contains("Unknown connector account"), "{error}");
    }

    #[test]
    fn validate_repo_segment_rejects_path_traversal_style_input() {
        assert!(validate_repo_segment("owner", "acme").is_ok());
        assert!(validate_repo_segment("owner", "../etc").is_err());
        assert!(validate_repo_segment("owner", "").is_err());
    }

    #[test]
    fn validate_git_ref_rejects_traversal_and_accepts_normal_branch_names() {
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("feature/add-thing").is_ok());
        assert!(validate_git_ref("../../etc/passwd").is_err());
    }

    #[test]
    fn validate_relative_prefix_rejects_absolute_and_traversal_paths() {
        assert!(validate_relative_prefix("prefix", "docs/guides").is_ok());
        assert!(validate_relative_prefix("prefix", "/etc").is_err());
        assert!(validate_relative_prefix("prefix", "../secrets").is_err());
    }

    #[test]
    fn connector_config_serde_tags_match_the_frontend_knowledge_connector_union() {
        let cases: Vec<(ConnectorConfig, &str)> = vec![
            (
                ConnectorConfig::GitHubRepo {
                    owner: "acme".into(),
                    repo: "widgets".into(),
                    git_ref: None,
                    path_prefix: None,
                    connector_account_id: "a".into(),
                },
                "git_hub_repo",
            ),
            (
                ConnectorConfig::S3Bucket {
                    endpoint: "https://s3.example.com".into(),
                    bucket: "bucket".into(),
                    prefix: None,
                    region: "us-east-1".into(),
                    connector_account_id: "a".into(),
                },
                "s3_bucket",
            ),
            (
                ConnectorConfig::WatchedFolder { path: "/tmp".into(), debounce_ms: 1_000 },
                "watched_folder",
            ),
            (
                ConnectorConfig::NotionPages { connector_account_id: "a".into(), root_id: "r".into() },
                "notion_pages",
            ),
            (
                ConnectorConfig::SlackChannels {
                    connector_account_id: "a".into(),
                    channel_ids: vec!["C1".into()],
                },
                "slack_channels",
            ),
            (
                ConnectorConfig::JiraProject {
                    connector_account_id: "a".into(),
                    project_key: "PROJ".into(),
                },
                "jira_project",
            ),
        ];
        for (config, expected_tag) in cases {
            let value = serde_json::to_value(&config).unwrap();
            assert_eq!(
                value.get("kind").and_then(Value::as_str),
                Some(expected_tag),
                "serde tag for {config:?} must match the frontend's KnowledgeConnector['kind']"
            );
        }
    }

    #[test]
    fn validate_notion_id_and_slack_channel_and_jira_key_reject_empty_and_odd_characters() {
        assert!(validate_notion_id("abcd1234-ef56-7890").is_ok());
        assert!(validate_notion_id("").is_err());
        assert!(validate_notion_id("has spaces").is_err());
        assert!(validate_slack_channel_id("C0123456789").is_ok());
        assert!(validate_slack_channel_id("").is_err());
        assert!(validate_jira_project_key("PROJ").is_ok());
        assert!(validate_jira_project_key("").is_err());
    }
}
