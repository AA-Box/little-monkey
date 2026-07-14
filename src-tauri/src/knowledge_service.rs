//! Persistent orchestration and Tauri commands for Knowledge Stacks 2.0.
//!
//! The active search generation is immutable. Refresh first enumerates and
//! validates every source, reuses prior chunk/vector rows only when both the
//! source hash and full pipeline fingerprint match, builds a staged SQLite
//! FTS/vector generation, and switches the active pointer last. Cancellation
//! or any connector/extractor/provider failure therefore leaves the previous
//! generation usable.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

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
}

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
    }
    let result = source.clone();
    save_catalog(&root, &catalog)?;
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
        let objects = collect_source_objects(app_data, &source, &limits, cancel).await?;
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
                let cursor_payload = objects
                    .iter()
                    .map(|object| (&object.object_id, &object.content_sha256))
                    .collect::<Vec<_>>();
                source.cursor = Some(sha256(
                    &serde_json::to_vec(&cursor_payload).map_err(|error| error.to_string())?,
                ));
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

async fn collect_source_objects(
    app_data: &Path,
    source: &KnowledgeSource,
    limits: &PipelineLimits,
    cancel: &CancellationToken,
) -> Result<Vec<SourceObject>, String> {
    match &source.connector {
        ConnectorConfig::LocalFile { path } => collect_local_file(source, Path::new(path), limits),
        ConnectorConfig::LocalFolder { path } | ConnectorConfig::Project { path } => {
            collect_local_folder(source, Path::new(path), limits, cancel)
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
        }
        ConnectorConfig::SelectedChats { session_ids } => {
            collect_selected_chats(app_data, source, session_ids, limits)
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
            Ok(vec![object])
        }
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
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .resolve(host, socket)
            .build()
            .map_err(|error| format!("Failed to create HTTP client: {error}"))?;
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
pub async fn query_for_agent(
    app: &AppHandle,
    stack: &KnowledgeStack,
    query: &str,
    k: usize,
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
    let response = index
        .search(
            query,
            &vector,
            &config,
            &PipelineLimits::default(),
            None,
            &CancellationToken::new(),
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
}
