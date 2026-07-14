//! M3 model/runtime/API integration service.
//!
//! This module deliberately contains no Tauri global state or HTTP listener.
//! It is the shared service boundary used by desktop commands, the local API,
//! and tests. Network, hardware, runtime-process, inference, secret-protection,
//! and clock effects are injected so callers can provide platform integrations
//! without weakening the validation and persistence rules here.

use crate::compatibility_hub::{
    compatibility_conformance_manifest, encode_response, encode_stream_event, ApiBackend, ApiScope,
    AuthorizationRequest, AuthorizedToken, CanonicalContent, CanonicalInferenceRequest,
    CanonicalInferenceResponse, CanonicalMessage, CanonicalRole, CanonicalStreamEvent,
    CanonicalUsage, CompatibilityConformanceManifest, CompatibilityError, CompatibilityProtocol,
    LanAccessController, LanEntropySource, LanServerPolicy, LanStateProtector, PairedToken,
    PairingChallengeView, PairingRequest, ProtocolStreamFrame, ScopedTokenView, SecurityAuditEvent,
};
use crate::mlx_runtime::{
    MlxGenerationRequest, MlxGenerationSummary, MlxMessage, MlxOperationContext, MlxProcessMetrics,
    MlxRuntimeAdapter, MlxRuntimeStatus, MlxStreamEvent, MlxStreamSink, MlxToolDefinition,
};
use crate::runtime_adapter::{
    validate_setting_values, AdvancedSettingCapability, HardwareProfile, HardwareSnapshot,
    KeepAlive, ModelLoadRequest, ModelUnloadRequest, RunningModel, RuntimeAdapter,
    RuntimeCapabilities, RuntimeInventory, RuntimeLogRequest, RuntimeLogTail,
    RuntimeOperationContext, RuntimeOperationLimits, RuntimeStatus, SettingValue, UnloadPolicy,
};
use reqwest::header::{
    HeaderValue, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;
use url::{Host, Url};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const M3_HUB_SCHEMA_VERSION: u32 = 1;
pub const M3_HUB_STATE_VERSION: u32 = 1;
pub const M3_CATALOG_SCHEMA_VERSION: u32 = 1;

const STATE_PREFIX: &str = "hub-state-";
const STATE_SUFFIX: &str = ".json";
const DOWNLOAD_SUFFIX: &str = ".partial";
const RESUME_SUFFIX: &str = ".resume.json";
const MODEL_PAYLOAD_FILE: &str = "model.bin";
const MODEL_MANIFEST_FILE: &str = "model.json";
const MAX_STATE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CATALOG_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CATALOG_ENTRIES: usize = 10_000;
const MAX_IDENTIFIER_BYTES: usize = 256;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_DOWNLOAD_CHUNK_BYTES: usize = 16 * 1024 * 1024;
const MIN_DOWNLOAD_CHUNK_BYTES: usize = 64 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
const MAX_RUNTIME_COUNT: usize = 128;
const MAX_INSTALLED_MODELS: usize = 10_000;
const MAX_MODEL_VERSIONS: usize = 32;
const KEPT_STATE_GENERATIONS: usize = 3;

pub type M3HubResult<T> = Result<T, M3HubError>;
pub type M3HubFuture<'a, T> = Pin<Box<dyn Future<Output = M3HubResult<T>> + Send + 'a>>;

#[derive(Debug)]
pub enum M3HubError {
    Invalid {
        field: String,
        message: String,
    },
    Unsupported(String),
    NotFound(String),
    Conflict(String),
    Unauthorized(String),
    Forbidden(String),
    RateLimited {
        retry_after_ms: u64,
    },
    Cancelled {
        operation: String,
    },
    Timeout {
        operation: String,
        timeout_ms: u64,
    },
    Integrity {
        expected: String,
        actual: String,
    },
    Storage {
        required: u64,
        available: u64,
    },
    Transport(String),
    Runtime(String),
    Compatibility(String),
    State(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    LockPoisoned,
}

impl fmt::Display for M3HubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { field, message } => write!(formatter, "invalid {field}: {message}"),
            Self::Unsupported(message) => write!(formatter, "unsupported: {message}"),
            Self::NotFound(message) => write!(formatter, "not found: {message}"),
            Self::Conflict(message) => write!(formatter, "conflict: {message}"),
            Self::Unauthorized(message) => write!(formatter, "unauthorized: {message}"),
            Self::Forbidden(message) => write!(formatter, "forbidden: {message}"),
            Self::RateLimited { retry_after_ms } => {
                write!(formatter, "rate limited; retry after {retry_after_ms} ms")
            }
            Self::Cancelled { operation } => write!(formatter, "{operation} was cancelled"),
            Self::Timeout {
                operation,
                timeout_ms,
            } => write!(formatter, "{operation} timed out after {timeout_ms} ms"),
            Self::Integrity { expected, actual } => {
                write!(
                    formatter,
                    "checksum mismatch: expected {expected}, got {actual}"
                )
            }
            Self::Storage {
                required,
                available,
            } => write!(
                formatter,
                "insufficient managed storage: need {required} bytes, have {available}"
            ),
            Self::Transport(message) => write!(formatter, "transport: {message}"),
            Self::Runtime(message) => write!(formatter, "runtime: {message}"),
            Self::Compatibility(message) => write!(formatter, "compatibility: {message}"),
            Self::State(message) => write!(formatter, "state: {message}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} at {}: {source}", path.display()),
            Self::Json(error) => write!(formatter, "JSON: {error}"),
            Self::LockPoisoned => write!(formatter, "shared state lock poisoned"),
        }
    }
}

impl std::error::Error for M3HubError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for M3HubError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<CompatibilityError> for M3HubError {
    fn from(error: CompatibilityError) -> Self {
        match error {
            CompatibilityError::Unauthorized(message) => Self::Unauthorized(message),
            CompatibilityError::Forbidden(message) => Self::Forbidden(message),
            CompatibilityError::RateLimited { retry_after_ms } => {
                Self::RateLimited { retry_after_ms }
            }
            other => Self::Compatibility(other.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3HubConfig {
    pub schema_version: u32,
    pub storage_quota_bytes: u64,
    pub storage_reserve_bytes: u64,
    pub download_chunk_bytes: usize,
    pub operation_timeout_ms: u64,
    pub max_catalog_results: usize,
}

impl Default for M3HubConfig {
    fn default() -> Self {
        Self {
            schema_version: M3_HUB_SCHEMA_VERSION,
            storage_quota_bytes: 250 * 1024 * 1024 * 1024,
            storage_reserve_bytes: 2 * 1024 * 1024 * 1024,
            download_chunk_bytes: 4 * 1024 * 1024,
            operation_timeout_ms: 120_000,
            max_catalog_results: 500,
        }
    }
}

impl M3HubConfig {
    pub fn validate(&self) -> M3HubResult<()> {
        if self.schema_version != M3_HUB_SCHEMA_VERSION {
            return Err(invalid("config.schemaVersion", "is unsupported"));
        }
        if self.storage_quota_bytes == 0
            || self.storage_quota_bytes > MAX_DOWNLOAD_BYTES
            || self.storage_reserve_bytes >= self.storage_quota_bytes
        {
            return Err(invalid(
                "config.storage",
                "quota must be positive and reserve must be smaller than quota",
            ));
        }
        if !(MIN_DOWNLOAD_CHUNK_BYTES..=MAX_DOWNLOAD_CHUNK_BYTES)
            .contains(&self.download_chunk_bytes)
        {
            return Err(invalid(
                "config.downloadChunkBytes",
                format!(
                    "must be between {MIN_DOWNLOAD_CHUNK_BYTES} and {MAX_DOWNLOAD_CHUNK_BYTES}"
                ),
            ));
        }
        if self.operation_timeout_ms == 0 || self.operation_timeout_ms > 60 * 60 * 1_000 {
            return Err(invalid(
                "config.operationTimeoutMs",
                "must be between 1 ms and 1 hour",
            ));
        }
        if self.max_catalog_results == 0 || self.max_catalog_results > MAX_CATALOG_ENTRIES {
            return Err(invalid(
                "config.maxCatalogResults",
                format!("must be between 1 and {MAX_CATALOG_ENTRIES}"),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct M3OperationContext {
    pub cancellation: CancellationToken,
    pub timeout_ms: u64,
}

impl M3OperationContext {
    pub fn new(timeout_ms: u64) -> Self {
        Self {
            cancellation: CancellationToken::new(),
            timeout_ms,
        }
    }

    fn preflight(&self, operation: &str) -> M3HubResult<()> {
        if self.timeout_ms == 0 || self.timeout_ms > 60 * 60 * 1_000 {
            return Err(invalid(
                "context.timeoutMs",
                "must be between 1 ms and 1 hour",
            ));
        }
        if self.cancellation.is_cancelled() {
            return Err(M3HubError::Cancelled {
                operation: operation.to_string(),
            });
        }
        Ok(())
    }
}

impl Default for M3OperationContext {
    fn default() -> Self {
        Self::new(M3HubConfig::default().operation_timeout_ms)
    }
}

pub trait M3Clock: Send + Sync {
    fn now_ms(&self) -> M3HubResult<u64>;
}

#[derive(Default)]
pub struct SystemM3Clock;

impl M3Clock for SystemM3Clock {
    fn now_ms(&self) -> M3HubResult<u64> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| M3HubError::State(format!("system clock before epoch: {error}")))?;
        u64::try_from(duration.as_millis())
            .map_err(|_| M3HubError::State("system timestamp overflow".to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3RuntimeKind {
    Ollama,
    LlamaCpp,
    Mlx,
}

impl M3RuntimeKind {
    fn api_backend(self) -> ApiBackend {
        match self {
            Self::Ollama => ApiBackend::Ollama,
            Self::LlamaCpp => ApiBackend::ManagedLocal,
            Self::Mlx => ApiBackend::Mlx,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ModelLicense {
    pub name: String,
    pub spdx_id: Option<String>,
    pub source_url: String,
    pub revision: String,
    pub retrieved_at_ms: u64,
    pub raw_declaration: String,
}

impl M3ModelLicense {
    pub fn declaration_sha256(&self) -> String {
        sha256_hex(self.raw_declaration.as_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ModelCapabilities {
    pub chat: bool,
    pub embeddings: bool,
    pub tool_calling: bool,
    pub vision: bool,
    pub structured_output: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CatalogModel {
    pub schema_version: u32,
    pub source_id: String,
    pub model_id: String,
    pub display_name: String,
    pub runtime: M3RuntimeKind,
    pub variant_id: String,
    pub revision: String,
    pub quantization: Option<String>,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub estimated_ram_bytes: u64,
    pub estimated_vram_bytes: u64,
    pub supported_os: BTreeSet<String>,
    pub supported_arch: BTreeSet<String>,
    pub required_accelerator: Option<String>,
    pub capabilities: M3ModelCapabilities,
    pub license: M3ModelLicense,
    pub metadata: BTreeMap<String, String>,
}

impl M3CatalogModel {
    pub fn asset_id(&self) -> String {
        format!(
            "{}:{}:{}",
            runtime_slug(self.runtime),
            self.model_id,
            self.variant_id
        )
    }

    fn asset_key(&self) -> String {
        sha256_hex(self.asset_id().as_bytes())
    }

    fn version_key(&self) -> String {
        sha256_hex(format!("{}\n{}\n{}", self.revision, self.sha256, self.download_url).as_bytes())
    }

    pub fn validate(&self) -> M3HubResult<()> {
        if self.schema_version != M3_CATALOG_SCHEMA_VERSION {
            return Err(invalid("catalog.schemaVersion", "is unsupported"));
        }
        for (field, value) in [
            ("sourceId", self.source_id.as_str()),
            ("modelId", self.model_id.as_str()),
            ("displayName", self.display_name.as_str()),
            ("variantId", self.variant_id.as_str()),
            ("revision", self.revision.as_str()),
        ] {
            validate_identifier(value, &format!("catalog.{field}"))?;
        }
        validate_sha256(&self.sha256, "catalog.sha256")?;
        if self.size_bytes == 0 || self.size_bytes > MAX_DOWNLOAD_BYTES {
            return Err(invalid(
                "catalog.sizeBytes",
                format!("must be between 1 and {MAX_DOWNLOAD_BYTES}"),
            ));
        }
        if self.estimated_ram_bytes == 0 {
            return Err(invalid("catalog.estimatedRamBytes", "must be positive"));
        }
        validate_download_url(&self.download_url)?;
        if self.supported_os.is_empty() || self.supported_arch.is_empty() {
            return Err(invalid(
                "catalog.platform",
                "supported OS and architecture sets must not be empty",
            ));
        }
        for value in self.supported_os.iter().chain(self.supported_arch.iter()) {
            validate_identifier(value, "catalog.platform")?;
        }
        if let Some(accelerator) = self.required_accelerator.as_deref() {
            parse_accelerator(accelerator)?;
        }
        validate_text(&self.license.name, "catalog.license.name", 16 * 1024)?;
        validate_text(
            &self.license.raw_declaration,
            "catalog.license.rawDeclaration",
            MAX_TEXT_BYTES,
        )?;
        if self.license.raw_declaration.trim().is_empty() {
            return Err(invalid(
                "catalog.license.rawDeclaration",
                "must not be empty",
            ));
        }
        validate_https_url(&self.license.source_url, "catalog.license.sourceUrl", false)?;
        validate_identifier(&self.license.revision, "catalog.license.revision")?;
        validate_timestamp(
            self.license.retrieved_at_ms,
            "catalog.license.retrievedAtMs",
        )?;
        if self.metadata.len() > 256 {
            return Err(invalid("catalog.metadata", "contains too many entries"));
        }
        for (key, value) in &self.metadata {
            validate_identifier(key, "catalog.metadata.key")?;
            validate_text(value, "catalog.metadata.value", 64 * 1024)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3HardwareFitRating {
    Recommended,
    Tight,
    TooLarge,
    Incompatible,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3HardwareFit {
    pub rating: M3HardwareFitRating,
    pub required_ram_bytes: u64,
    pub available_ram_bytes: u64,
    pub required_vram_bytes: u64,
    pub available_vram_bytes: u64,
    pub reasons: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CatalogMatch {
    pub model: M3CatalogModel,
    pub fit: M3HardwareFit,
}

pub trait M3HardwareProbe: Send + Sync {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot>;
}

pub trait M3CatalogSource: Send + Sync {
    fn source_id(&self) -> &str;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<M3CatalogModel>>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3CatalogEnvelope {
    schema_version: u32,
    entries: Vec<M3CatalogModel>,
}

pub struct HttpM3CatalogSource {
    source_id: String,
    endpoint: Url,
    client: reqwest::Client,
    max_body_bytes: usize,
}

impl HttpM3CatalogSource {
    pub fn new(source_id: impl Into<String>, endpoint: &str) -> M3HubResult<Self> {
        let source_id = source_id.into();
        validate_identifier(&source_id, "catalogSource.sourceId")?;
        validate_https_url(endpoint, "catalogSource.endpoint", true)?;
        let endpoint = Url::parse(endpoint)
            .map_err(|error| invalid("catalogSource.endpoint", error.to_string()))?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            source_id,
            endpoint,
            client,
            max_body_bytes: MAX_CATALOG_BODY_BYTES,
        })
    }
}

impl M3CatalogSource for HttpM3CatalogSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<M3CatalogModel>> {
        Box::pin(async move {
            context.preflight("catalog search")?;
            validate_search(query, limit)?;
            let mut url = self.endpoint.clone();
            url.query_pairs_mut()
                .append_pair("q", query)
                .append_pair("limit", &limit.to_string());
            let response = run_bounded(context, "catalog search", async {
                self.client
                    .get(url)
                    .send()
                    .await
                    .map_err(|error| M3HubError::Transport(error.to_string()))
            })
            .await?;
            if !response.status().is_success() {
                return Err(M3HubError::Transport(format!(
                    "catalog returned HTTP {}",
                    response.status()
                )));
            }
            let bytes = read_response_bounded(response, self.max_body_bytes, context).await?;
            let envelope: M3CatalogEnvelope = serde_json::from_slice(&bytes)?;
            if envelope.schema_version != M3_CATALOG_SCHEMA_VERSION {
                return Err(invalid("catalog.schemaVersion", "is unsupported"));
            }
            if envelope.entries.len() > limit || envelope.entries.len() > MAX_CATALOG_ENTRIES {
                return Err(invalid(
                    "catalog.entries",
                    "response exceeds the requested result limit",
                ));
            }
            for entry in &envelope.entries {
                entry.validate()?;
                if entry.source_id != self.source_id {
                    return Err(invalid(
                        "catalog.sourceId",
                        "entry source differs from the configured source",
                    ));
                }
            }
            Ok(envelope.entries)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M3DownloadProbe {
    pub total_bytes: u64,
    pub etag: Option<String>,
    pub accepts_ranges: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M3DownloadChunk {
    pub offset: u64,
    pub total_bytes: u64,
    pub etag: Option<String>,
    pub bytes: Vec<u8>,
}

pub trait M3DownloadTransport: Send + Sync {
    fn probe<'a>(
        &'a self,
        url: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadProbe>;

    fn read_range<'a>(
        &'a self,
        url: &'a str,
        offset: u64,
        max_bytes: usize,
        expected_etag: Option<&'a str>,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadChunk>;
}

pub struct ReqwestM3DownloadTransport {
    client: reqwest::Client,
}

impl ReqwestM3DownloadTransport {
    pub fn new() -> M3HubResult<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self { client })
    }
}

impl M3DownloadTransport for ReqwestM3DownloadTransport {
    fn probe<'a>(
        &'a self,
        url: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadProbe> {
        Box::pin(async move {
            validate_download_url(url)?;
            context.preflight("probe model download")?;
            let response = run_bounded(context, "probe model download", async {
                self.client
                    .head(url)
                    .send()
                    .await
                    .map_err(|error| M3HubError::Transport(error.to_string()))
            })
            .await?;
            if !response.status().is_success() {
                return Err(M3HubError::Transport(format!(
                    "download probe returned HTTP {}",
                    response.status()
                )));
            }
            let total_bytes = header_u64(response.headers().get(CONTENT_LENGTH), "content-length")?;
            if total_bytes == 0 || total_bytes > MAX_DOWNLOAD_BYTES {
                return Err(invalid("download.contentLength", "is invalid"));
            }
            let etag = optional_header(response.headers().get(ETAG), "etag")?;
            let accepts_ranges = response
                .headers()
                .get(ACCEPT_RANGES)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.eq_ignore_ascii_case("bytes"));
            Ok(M3DownloadProbe {
                total_bytes,
                etag,
                accepts_ranges,
            })
        })
    }

    fn read_range<'a>(
        &'a self,
        url: &'a str,
        offset: u64,
        max_bytes: usize,
        expected_etag: Option<&'a str>,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadChunk> {
        Box::pin(async move {
            validate_download_url(url)?;
            context.preflight("download model range")?;
            if max_bytes == 0 || max_bytes > MAX_DOWNLOAD_CHUNK_BYTES {
                return Err(invalid("download.maxBytes", "is outside the safe range"));
            }
            let end = offset
                .checked_add(max_bytes as u64 - 1)
                .ok_or_else(|| invalid("download.range", "overflow"))?;
            let mut request = self
                .client
                .get(url)
                .header(RANGE, format!("bytes={offset}-{end}"));
            if let Some(etag) = expected_etag {
                request = request.header(IF_RANGE, etag);
            }
            let mut response = run_bounded(context, "download model range", async {
                request
                    .send()
                    .await
                    .map_err(|error| M3HubError::Transport(error.to_string()))
            })
            .await?;
            if response.status() != reqwest::StatusCode::PARTIAL_CONTENT {
                return Err(M3HubError::Transport(format!(
                    "range request returned HTTP {}; byte-range support is required",
                    response.status()
                )));
            }
            let (actual_offset, declared_end, total_bytes) = parse_content_range(
                response
                    .headers()
                    .get(CONTENT_RANGE)
                    .ok_or_else(|| invalid("download.contentRange", "is missing"))?,
            )?;
            let etag = optional_header(response.headers().get(ETAG), "etag")?;
            let mut bytes = Vec::new();
            while let Some(chunk) = run_bounded(context, "read model range", async {
                response
                    .chunk()
                    .await
                    .map_err(|error| M3HubError::Transport(error.to_string()))
            })
            .await?
            {
                if bytes.len().saturating_add(chunk.len()) > max_bytes {
                    return Err(invalid(
                        "download.rangeBody",
                        "server exceeded the requested bounded range",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            if bytes.is_empty() {
                return Err(M3HubError::Transport(
                    "range response contained no bytes".to_string(),
                ));
            }
            let declared_bytes = declared_end
                .checked_sub(actual_offset)
                .and_then(|value| value.checked_add(1))
                .ok_or_else(|| invalid("download.contentRange", "contains an invalid range"))?;
            if bytes.len() as u64 != declared_bytes {
                return Err(invalid(
                    "download.contentRange",
                    "body length differs from the declared byte range",
                ));
            }
            Ok(M3DownloadChunk {
                offset: actual_offset,
                total_bytes,
                etag,
                bytes,
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3StoredModelVersion {
    version_key: String,
    model: M3CatalogModel,
    artifact_relative_path: String,
    installed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3StoredModel {
    asset_id: String,
    asset_key: String,
    active_version_key: String,
    versions: Vec<M3StoredModelVersion>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3HubState {
    state_version: u32,
    generation: u64,
    updated_at_ms: u64,
    models: Vec<M3StoredModel>,
    runtime_configs: BTreeMap<String, BTreeMap<String, SettingValue>>,
    lan_policy: Option<LanServerPolicy>,
}

impl Default for M3HubState {
    fn default() -> Self {
        Self {
            state_version: M3_HUB_STATE_VERSION,
            generation: 0,
            updated_at_ms: 0,
            models: Vec::new(),
            runtime_configs: BTreeMap::new(),
            lan_policy: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3ResumeState {
    schema_version: u32,
    asset_key: String,
    version_key: String,
    url: String,
    expected_sha256: String,
    total_bytes: u64,
    etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3DownloadRequest {
    pub model: M3CatalogModel,
    pub accepted_license_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3InstalledVersionView {
    pub version_key: String,
    pub revision: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub artifact_path: PathBuf,
    pub installed_at_ms: u64,
    pub active: bool,
    pub license: M3ModelLicense,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3InstalledModelView {
    pub asset_id: String,
    pub model_id: String,
    pub display_name: String,
    pub runtime: M3RuntimeKind,
    pub variant_id: String,
    pub capabilities: M3ModelCapabilities,
    pub estimated_ram_bytes: u64,
    pub estimated_vram_bytes: u64,
    pub required_accelerator: Option<String>,
    pub active_version_key: String,
    pub versions: Vec<M3InstalledVersionView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3StorageStatus {
    pub root: PathBuf,
    pub quota_bytes: u64,
    pub reserve_bytes: u64,
    pub used_bytes: u64,
    pub available_for_models_bytes: u64,
    pub pending_download_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CleanupReport {
    pub removed_paths: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct M3ResolvedModel {
    pub asset_id: String,
    pub model_id: String,
    pub runtime: M3RuntimeKind,
    pub artifact_path: PathBuf,
    pub size_bytes: u64,
    pub capabilities: M3ModelCapabilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3LoadModelRequest {
    pub runtime_id: String,
    pub asset_id: String,
    pub keep_alive: Option<KeepAlive>,
    pub replace_existing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3UnloadModelRequest {
    pub runtime_id: String,
    pub model_id: String,
    pub force_exact_owner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3DeleteModelRequest {
    pub asset_id: String,
    pub confirmation: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ActivateModelVersionRequest {
    pub asset_id: String,
    pub version_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3PruneModelVersionsRequest {
    pub asset_id: String,
    pub confirmation: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3SetRuntimeConfigRequest {
    pub runtime_id: String,
    pub values: BTreeMap<String, SettingValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3RuntimeDescriptor {
    pub runtime_id: String,
    pub kind: M3RuntimeKind,
    pub label: String,
    pub managed: bool,
    pub api_backend: ApiBackend,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3RuntimeCapabilityView {
    pub descriptor: M3RuntimeDescriptor,
    pub can_load: bool,
    pub can_unload: bool,
    pub can_logs: bool,
    pub can_metrics: bool,
    pub can_infer: bool,
    pub settings: Vec<AdvancedSettingCapability>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "runtimeType", rename_all = "snake_case")]
pub enum M3RuntimeStatusView {
    Adapter {
        status: RuntimeStatus,
        running_models: Vec<RunningModel>,
    },
    Mlx {
        status: MlxRuntimeStatus,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "runtimeType", rename_all = "snake_case")]
pub enum M3RuntimeMetricsView {
    Adapter {
        status: RuntimeStatus,
        running_models: Vec<RunningModel>,
    },
    Mlx {
        metrics: Option<MlxProcessMetrics>,
        status: MlxRuntimeStatus,
    },
}

pub trait M3CanonicalStreamSink: Send {
    fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String>;
}

pub trait M3ProtocolFrameSink: Send {
    fn emit(&mut self, frame: ProtocolStreamFrame) -> Result<(), String>;
}

pub trait M3InferenceEngine: Send + Sync {
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse>;

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()>;

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool>;
}

pub trait M3RuntimeDriver: Send + Sync {
    fn descriptor(&self) -> M3RuntimeDescriptor;
    fn capabilities(&self) -> M3RuntimeCapabilityView;
    fn validate_config(&self, values: &BTreeMap<String, SettingValue>) -> M3HubResult<()>;
    fn status<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeStatusView>;
    fn inventory<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeInventory>;
    fn load<'a>(
        &'a self,
        model: &'a M3ResolvedModel,
        settings: &'a BTreeMap<String, SettingValue>,
        keep_alive: Option<KeepAlive>,
        replace_existing: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()>;
    fn unload<'a>(
        &'a self,
        model_id: &'a str,
        force_exact_owner: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()>;
    fn logs<'a>(
        &'a self,
        max_bytes: usize,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeLogTail>;
    fn metrics<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeMetricsView>;
    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse>;
    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()>;
    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool>;
}

pub struct RuntimeAdapterM3Driver {
    adapter: Arc<dyn RuntimeAdapter>,
    inference: Arc<dyn M3InferenceEngine>,
    descriptor: M3RuntimeDescriptor,
}

impl RuntimeAdapterM3Driver {
    pub fn new(
        adapter: Arc<dyn RuntimeAdapter>,
        inference: Arc<dyn M3InferenceEngine>,
    ) -> M3HubResult<Self> {
        let source = adapter.descriptor();
        let kind = match source.kind {
            crate::runtime_adapter::RuntimeKind::Ollama => M3RuntimeKind::Ollama,
            crate::runtime_adapter::RuntimeKind::LlamaCpp => M3RuntimeKind::LlamaCpp,
        };
        let descriptor = M3RuntimeDescriptor {
            runtime_id: source.runtime_id,
            kind,
            label: source.label,
            managed: source.managed,
            api_backend: kind.api_backend(),
        };
        validate_identifier(&descriptor.runtime_id, "runtime.runtimeId")?;
        Ok(Self {
            adapter,
            inference,
            descriptor,
        })
    }

    fn runtime_context(
        &self,
        context: &M3OperationContext,
    ) -> M3HubResult<RuntimeOperationContext> {
        context.preflight("runtime operation")?;
        let limits = RuntimeOperationLimits {
            timeout_ms: context.timeout_ms,
            ..RuntimeOperationLimits::default()
        };
        limits.validate().map_err(runtime_error)?;
        Ok(RuntimeOperationContext::new(
            limits,
            context.cancellation.clone(),
        ))
    }
}

impl M3RuntimeDriver for RuntimeAdapterM3Driver {
    fn descriptor(&self) -> M3RuntimeDescriptor {
        self.descriptor.clone()
    }

    fn capabilities(&self) -> M3RuntimeCapabilityView {
        let source = self.adapter.capabilities();
        M3RuntimeCapabilityView {
            descriptor: self.descriptor(),
            can_load: source.can_load,
            can_unload: source.can_unload,
            can_logs: source.can_tail_logs,
            can_metrics: true,
            can_infer: true,
            settings: source.settings,
        }
    }

    fn validate_config(&self, values: &BTreeMap<String, SettingValue>) -> M3HubResult<()> {
        let capabilities: RuntimeCapabilities = self.adapter.capabilities();
        validate_setting_values(
            &self.descriptor.runtime_id,
            &capabilities.settings,
            values,
            RuntimeOperationLimits::default().max_config_bytes,
        )
        .map_err(runtime_error)
    }

    fn status<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeStatusView> {
        Box::pin(async move {
            let runtime_context = self.runtime_context(context)?;
            let status = self
                .adapter
                .status(&runtime_context)
                .await
                .map_err(runtime_error)?;
            let running_models = self
                .adapter
                .running_models(&runtime_context)
                .await
                .map_err(runtime_error)?;
            Ok(M3RuntimeStatusView::Adapter {
                status,
                running_models,
            })
        })
    }

    fn inventory<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            let runtime_context = self.runtime_context(context)?;
            self.adapter
                .inventory(&runtime_context)
                .await
                .map_err(runtime_error)
        })
    }

    fn load<'a>(
        &'a self,
        model: &'a M3ResolvedModel,
        settings: &'a BTreeMap<String, SettingValue>,
        keep_alive: Option<KeepAlive>,
        replace_existing: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            if model.runtime != self.descriptor.kind {
                return Err(M3HubError::Conflict(
                    "model runtime differs from selected runtime".to_string(),
                ));
            }
            self.validate_config(settings)?;
            let runtime_context = self.runtime_context(context)?;
            let inventory = self
                .adapter
                .inventory(&runtime_context)
                .await
                .map_err(runtime_error)?;
            let known = inventory.models.iter().any(|candidate| {
                candidate.model_id == model.model_id
                    && candidate
                        .local_path
                        .as_ref()
                        .is_none_or(|path| path == &model.artifact_path)
            });
            if !known {
                return Err(M3HubError::Conflict(format!(
                    "runtime {} has not reconciled installed model {}",
                    self.descriptor.runtime_id, model.model_id
                )));
            }
            self.adapter
                .load_model(
                    &ModelLoadRequest {
                        model_id: model.model_id.clone(),
                        keep_alive,
                        settings: settings.clone(),
                        replace_existing,
                    },
                    &runtime_context,
                )
                .await
                .map_err(runtime_error)?;
            Ok(())
        })
    }

    fn unload<'a>(
        &'a self,
        model_id: &'a str,
        force_exact_owner: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let runtime_context = self.runtime_context(context)?;
            self.adapter
                .unload_model(
                    &ModelUnloadRequest {
                        model_id: model_id.to_string(),
                        policy: if force_exact_owner {
                            UnloadPolicy::ExactRegardlessOfOwner
                        } else {
                            UnloadPolicy::AppManagedOnly
                        },
                    },
                    &runtime_context,
                )
                .await
                .map_err(runtime_error)?;
            Ok(())
        })
    }

    fn logs<'a>(
        &'a self,
        max_bytes: usize,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            let runtime_context = self.runtime_context(context)?;
            self.adapter
                .tail_logs(&RuntimeLogRequest { max_bytes }, &runtime_context)
                .await
                .map_err(runtime_error)
        })
    }

    fn metrics<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeMetricsView> {
        Box::pin(async move {
            let runtime_context = self.runtime_context(context)?;
            let status = self
                .adapter
                .status(&runtime_context)
                .await
                .map_err(runtime_error)?;
            let running_models = self
                .adapter
                .running_models(&runtime_context)
                .await
                .map_err(runtime_error)?;
            Ok(M3RuntimeMetricsView::Adapter {
                status,
                running_models,
            })
        })
    }

    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        self.inference.complete(request, context)
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        self.inference.stream(request, sink, context)
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        self.inference.cancel(request_id, context)
    }
}

pub struct MlxM3Driver {
    runtime_id: String,
    adapter: Arc<MlxRuntimeAdapter>,
    clock: Arc<dyn M3Clock>,
}

impl MlxM3Driver {
    pub fn new(
        runtime_id: impl Into<String>,
        adapter: Arc<MlxRuntimeAdapter>,
        clock: Arc<dyn M3Clock>,
    ) -> M3HubResult<Self> {
        let runtime_id = runtime_id.into();
        validate_identifier(&runtime_id, "runtime.runtimeId")?;
        Ok(Self {
            runtime_id,
            adapter,
            clock,
        })
    }

    fn mlx_context(&self, context: &M3OperationContext) -> M3HubResult<MlxOperationContext> {
        context.preflight("MLX operation")?;
        Ok(MlxOperationContext {
            cancellation: context.cancellation.clone(),
            timeout_ms: context.timeout_ms,
        })
    }

    fn generation_request(
        &self,
        request: &CanonicalInferenceRequest,
    ) -> M3HubResult<MlxGenerationRequest> {
        let messages = request
            .messages
            .iter()
            .map(canonical_message_to_mlx)
            .collect::<M3HubResult<Vec<_>>>()?;
        let tools = request
            .tools
            .iter()
            .map(|tool| MlxToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.clone(),
            })
            .collect();
        Ok(MlxGenerationRequest {
            request_id: request.request_id.clone(),
            model_id: request.model.clone(),
            messages,
            tools,
            max_tokens: request.max_output_tokens,
            temperature: request.temperature,
            structured_output_schema: request.response_schema.clone(),
        })
    }
}

impl M3RuntimeDriver for MlxM3Driver {
    fn descriptor(&self) -> M3RuntimeDescriptor {
        M3RuntimeDescriptor {
            runtime_id: self.runtime_id.clone(),
            kind: M3RuntimeKind::Mlx,
            label: "MLX".to_string(),
            managed: true,
            api_backend: ApiBackend::Mlx,
        }
    }

    fn capabilities(&self) -> M3RuntimeCapabilityView {
        let host_available = self
            .adapter
            .capabilities()
            .is_ok_and(|capabilities| capabilities.is_available());
        let installed = host_available && self.adapter.has_verified_install();
        M3RuntimeCapabilityView {
            descriptor: self.descriptor(),
            can_load: installed,
            can_unload: installed,
            can_logs: installed,
            can_metrics: installed,
            can_infer: installed,
            settings: Vec::new(),
        }
    }

    fn validate_config(&self, values: &BTreeMap<String, SettingValue>) -> M3HubResult<()> {
        if values.is_empty() {
            Ok(())
        } else {
            Err(M3HubError::Unsupported(
                "MLX advanced settings are capability-driven by its managed package".to_string(),
            ))
        }
    }

    fn status<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeStatusView> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            let status = self.adapter.status(&mlx_context).await.map_err(mlx_error)?;
            Ok(M3RuntimeStatusView::Mlx { status })
        })
    }

    fn inventory<'a>(
        &'a self,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeInventory> {
        Box::pin(async move {
            let models = self
                .adapter
                .models()
                .into_iter()
                .map(|model| crate::runtime_adapter::RuntimeModel {
                    model_id: model.model_id,
                    display_name: model.display_name,
                    size_bytes: model.size_bytes,
                    local_path: Some(model.local_path),
                    digest: None,
                    modified_at: model.revision,
                    capabilities: crate::runtime_adapter::ModelCapabilities {
                        chat: model.capabilities.chat,
                        embeddings: false,
                        tool_calling: model.capabilities.tool_calling,
                        vision: model.capabilities.vision,
                    },
                    metadata: BTreeMap::from([(
                        "structured_output".to_string(),
                        model.capabilities.structured_output.to_string(),
                    )]),
                })
                .collect();
            Ok(RuntimeInventory {
                schema_version: crate::runtime_adapter::RUNTIME_ADAPTER_SCHEMA_VERSION,
                runtime_id: self.runtime_id.clone(),
                models,
                captured_at_ms: self.clock.now_ms()?,
            })
        })
    }

    fn load<'a>(
        &'a self,
        model: &'a M3ResolvedModel,
        settings: &'a BTreeMap<String, SettingValue>,
        _keep_alive: Option<KeepAlive>,
        _replace_existing: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            if model.runtime != M3RuntimeKind::Mlx {
                return Err(M3HubError::Conflict(
                    "non-MLX model selected for MLX runtime".to_string(),
                ));
            }
            self.validate_config(settings)?;
            let known = self.adapter.models().iter().any(|candidate| {
                candidate.model_id == model.model_id && candidate.local_path == model.artifact_path
            });
            if !known {
                return Err(M3HubError::Conflict(format!(
                    "MLX adapter has not reconciled installed model {}",
                    model.model_id
                )));
            }
            let mlx_context = self.mlx_context(context)?;
            self.adapter
                .start(&model.model_id, &mlx_context)
                .await
                .map_err(mlx_error)?;
            Ok(())
        })
    }

    fn unload<'a>(
        &'a self,
        model_id: &'a str,
        _force_exact_owner: bool,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            if let MlxRuntimeStatus::Running { handle, .. } =
                self.adapter.status(&mlx_context).await.map_err(mlx_error)?
            {
                if handle.model_id != model_id {
                    return Err(M3HubError::Conflict(format!(
                        "MLX currently owns model {}; refusing to unload it as {model_id}",
                        handle.model_id
                    )));
                }
                self.adapter.unload(&mlx_context).await.map_err(mlx_error)?;
            }
            Ok(())
        })
    }

    fn logs<'a>(
        &'a self,
        max_bytes: usize,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, RuntimeLogTail> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            let text = self
                .adapter
                .tail_logs(max_bytes, &mlx_context)
                .await
                .map_err(mlx_error)?;
            Ok(RuntimeLogTail {
                truncated: text.len() == max_bytes,
                text,
            })
        })
    }

    fn metrics<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3RuntimeMetricsView> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            let status = self.adapter.status(&mlx_context).await.map_err(mlx_error)?;
            let metrics = if matches!(status, MlxRuntimeStatus::Running { .. }) {
                Some(
                    self.adapter
                        .metrics(&mlx_context)
                        .await
                        .map_err(mlx_error)?,
                )
            } else {
                None
            };
            Ok(M3RuntimeMetricsView::Mlx { metrics, status })
        })
    }

    fn complete<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalInferenceResponse> {
        Box::pin(async move {
            let mut collector = CanonicalCollector::default();
            self.stream(request, &mut collector, context).await?;
            collector.into_response(request, self.clock.now_ms()? / 1_000)
        })
    }

    fn stream<'a>(
        &'a self,
        request: &'a CanonicalInferenceRequest,
        sink: &'a mut dyn M3CanonicalStreamSink,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, ()> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            self.adapter
                .start(&request.model, &mlx_context)
                .await
                .map_err(mlx_error)?;
            let response_id = format!("resp-{}", request.request_id);
            sink.emit(CanonicalStreamEvent::ResponseStart {
                response_id: response_id.clone(),
                model: request.model.clone(),
                created_at_seconds: self.clock.now_ms()? / 1_000,
            })
            .map_err(stream_sink_error)?;
            let generation = self.generation_request(request)?;
            let mut adapter_sink = MlxCanonicalSink::new(sink, response_id);
            let summary = self
                .adapter
                .stream(&generation, &mut adapter_sink, &mlx_context)
                .await
                .map_err(mlx_error)?;
            adapter_sink.finish_if_needed(&summary)?;
            Ok(())
        })
    }

    fn cancel<'a>(
        &'a self,
        request_id: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, bool> {
        Box::pin(async move {
            let mlx_context = self.mlx_context(context)?;
            self.adapter
                .cancel_generation(request_id, &mlx_context)
                .await
                .map_err(mlx_error)
        })
    }
}

pub trait M3RuntimeReconciler: Send + Sync {
    fn reconcile<'a>(
        &'a self,
        installed: &'a [M3InstalledModelView],
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<Arc<dyn M3RuntimeDriver>>>;
}

pub trait M3LanAccessFactory: Send + Sync {
    fn create(
        &self,
        state_root: &Path,
        policy: LanServerPolicy,
    ) -> M3HubResult<Arc<LanAccessController>>;
}

pub struct DefaultM3LanAccessFactory {
    entropy: Arc<dyn LanEntropySource>,
    protector: Arc<dyn LanStateProtector>,
}

impl DefaultM3LanAccessFactory {
    pub fn new(entropy: Arc<dyn LanEntropySource>, protector: Arc<dyn LanStateProtector>) -> Self {
        Self { entropy, protector }
    }
}

impl M3LanAccessFactory for DefaultM3LanAccessFactory {
    fn create(
        &self,
        state_root: &Path,
        policy: LanServerPolicy,
    ) -> M3HubResult<Arc<LanAccessController>> {
        LanAccessController::new(
            state_root,
            policy,
            self.entropy.clone(),
            self.protector.clone(),
        )
        .map(Arc::new)
        .map_err(M3HubError::from)
    }
}

pub struct M3RuntimeHubDependencies {
    pub clock: Arc<dyn M3Clock>,
    pub hardware: Arc<dyn M3HardwareProbe>,
    pub download: Arc<dyn M3DownloadTransport>,
    pub catalogs: Vec<Arc<dyn M3CatalogSource>>,
    pub runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
    pub runtime_reconciler: Option<Arc<dyn M3RuntimeReconciler>>,
    pub lan_factory: Option<Arc<dyn M3LanAccessFactory>>,
}

pub struct M3RuntimeHub {
    config: M3HubConfig,
    root: PathBuf,
    models_root: PathBuf,
    downloads_root: PathBuf,
    state_root: PathBuf,
    lan_state_root: PathBuf,
    clock: Arc<dyn M3Clock>,
    hardware: Arc<dyn M3HardwareProbe>,
    download: Arc<dyn M3DownloadTransport>,
    catalogs: RwLock<Vec<Arc<dyn M3CatalogSource>>>,
    runtimes: RwLock<BTreeMap<String, Arc<dyn M3RuntimeDriver>>>,
    runtime_reconciler: Option<Arc<dyn M3RuntimeReconciler>>,
    lan_factory: Option<Arc<dyn M3LanAccessFactory>>,
    lan: RwLock<Option<Arc<LanAccessController>>>,
    state_lock: Mutex<()>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl M3RuntimeHub {
    pub fn new(
        root: impl AsRef<Path>,
        config: M3HubConfig,
        dependencies: M3RuntimeHubDependencies,
    ) -> M3HubResult<Self> {
        config.validate()?;
        validate_catalog_sources(&dependencies.catalogs)?;
        let root = root.as_ref().to_path_buf();
        if !root.is_absolute() {
            return Err(invalid("root", "must be an absolute app-private path"));
        }
        ensure_private_directory(&root)?;
        let models_root = root.join("models");
        let downloads_root = root.join("downloads");
        let state_root = root.join("state");
        let lan_state_root = root.join("lan-security");
        for directory in [&models_root, &downloads_root, &state_root, &lan_state_root] {
            ensure_private_directory(directory)?;
        }
        let state = load_hub_state(&state_root, &models_root)?;
        let runtimes = runtime_map(dependencies.runtimes)?;
        let lan = match (&state.lan_policy, &dependencies.lan_factory) {
            (Some(policy), Some(factory)) => Some(factory.create(&lan_state_root, policy.clone())?),
            (Some(_), None) => {
                return Err(M3HubError::State(
                    "persisted LAN policy requires a LAN access factory".to_string(),
                ))
            }
            (None, _) => None,
        };
        Ok(Self {
            config,
            root,
            models_root,
            downloads_root,
            state_root,
            lan_state_root,
            clock: dependencies.clock,
            hardware: dependencies.hardware,
            download: dependencies.download,
            catalogs: RwLock::new(dependencies.catalogs),
            runtimes: RwLock::new(runtimes),
            runtime_reconciler: dependencies.runtime_reconciler,
            lan_factory: dependencies.lan_factory,
            lan: RwLock::new(lan),
            state_lock: Mutex::new(()),
            mutation_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> &M3HubConfig {
        &self.config
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn replace_catalog_sources(
        &self,
        sources: Vec<Arc<dyn M3CatalogSource>>,
    ) -> M3HubResult<()> {
        validate_catalog_sources(&sources)?;
        *write_lock(&self.catalogs)? = sources;
        Ok(())
    }

    pub fn conformance_manifest(&self) -> CompatibilityConformanceManifest {
        compatibility_conformance_manifest()
    }

    pub fn hardware_snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        let snapshot = self.hardware.snapshot()?;
        snapshot.profile().map_err(runtime_error)?;
        Ok(snapshot)
    }

    pub fn hardware_profile(&self) -> M3HubResult<HardwareProfile> {
        self.hardware_snapshot()?.profile().map_err(runtime_error)
    }

    pub fn storage_status(&self) -> M3HubResult<M3StorageStatus> {
        let model_bytes = directory_size(&self.models_root)?;
        let pending_download_bytes = directory_size(&self.downloads_root)?;
        let used_bytes = model_bytes
            .checked_add(pending_download_bytes)
            .ok_or_else(|| M3HubError::State("managed storage byte count overflow".to_string()))?;
        let available_for_models_bytes = self
            .config
            .storage_quota_bytes
            .saturating_sub(self.config.storage_reserve_bytes)
            .saturating_sub(used_bytes);
        Ok(M3StorageStatus {
            root: self.root.clone(),
            quota_bytes: self.config.storage_quota_bytes,
            reserve_bytes: self.config.storage_reserve_bytes,
            used_bytes,
            available_for_models_bytes,
            pending_download_bytes,
        })
    }

    /// Remove only paths whose names are generated by this hub for resumable
    /// downloads, staging, or isolated trash. Unknown directories are never
    /// guessed to be ours. The mutation lock ensures an active download or
    /// lifecycle transition cannot be mistaken for an orphan.
    pub async fn cleanup_orphans(
        &self,
        confirmation: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<M3CleanupReport> {
        if confirmation != "CLEAN ORPHANS" {
            return Err(M3HubError::Forbidden(
                "orphan cleanup requires exact confirmation".to_string(),
            ));
        }
        context.preflight("clean runtime hub orphans")?;
        let _mutation = self.mutation_lock.lock().await;
        let state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let mut removed_paths = 0_usize;
        let mut reclaimed_bytes = 0_u64;

        for entry in fs::read_dir(&self.downloads_root)
            .map_err(|source| io_at("list interrupted downloads", &self.downloads_root, source))?
        {
            let entry = entry.map_err(|source| {
                io_at("read interrupted download", &self.downloads_root, source)
            })?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !(name.ends_with(DOWNLOAD_SUFFIX) || name.ends_with(RESUME_SUFFIX)) {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| io_at("inspect interrupted download", &path, source))?;
            if !metadata.file_type().is_file() {
                return Err(M3HubError::State(
                    "interrupted download path is not a regular file".to_string(),
                ));
            }
            reclaimed_bytes = reclaimed_bytes.saturating_add(metadata.len());
            remove_owned_file(&path)?;
            removed_paths += 1;
        }

        for entry in fs::read_dir(&self.models_root)
            .map_err(|source| io_at("list model cleanup roots", &self.models_root, source))?
        {
            let entry = entry
                .map_err(|source| io_at("read model cleanup root", &self.models_root, source))?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(".trash-") {
                reclaimed_bytes = reclaimed_bytes.saturating_add(directory_size(&path)?);
                remove_owned_directory(&path)?;
                removed_paths += 1;
            }
        }

        for model in &state.models {
            let asset_root = self.models_root.join(&model.asset_key);
            for entry in fs::read_dir(&asset_root)
                .map_err(|source| io_at("list model staging paths", &asset_root, source))?
            {
                let entry = entry
                    .map_err(|source| io_at("read model staging path", &asset_root, source))?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(".staging-") {
                    continue;
                }
                let path = entry.path();
                reclaimed_bytes = reclaimed_bytes.saturating_add(directory_size(&path)?);
                remove_owned_directory(&path)?;
                removed_paths += 1;
            }
        }
        sync_directory(&self.downloads_root)?;
        sync_directory(&self.models_root)?;
        Ok(M3CleanupReport {
            removed_paths,
            reclaimed_bytes,
        })
    }

    pub async fn search_catalog(
        &self,
        query: &str,
        limit: usize,
        context: &M3OperationContext,
    ) -> M3HubResult<Vec<M3CatalogMatch>> {
        context.preflight("catalog search")?;
        validate_search(query, limit)?;
        if limit > self.config.max_catalog_results {
            return Err(invalid(
                "limit",
                "exceeds the configured catalog result limit",
            ));
        }
        let hardware = self.hardware_snapshot()?;
        let mut matches = Vec::new();
        let mut dedupe = BTreeSet::new();
        let catalogs = read_lock(&self.catalogs)?.clone();
        for source in &catalogs {
            let remaining = limit.saturating_sub(matches.len());
            if remaining == 0 {
                break;
            }
            let entries = run_bounded(
                context,
                "catalog source search",
                source.search(query, remaining, context),
            )
            .await?;
            if entries.len() > remaining {
                return Err(invalid(
                    "catalog.entries",
                    "source returned more entries than requested",
                ));
            }
            for entry in entries {
                entry.validate()?;
                if entry.source_id != source.source_id() {
                    return Err(invalid(
                        "catalog.sourceId",
                        "source returned an entry for another source",
                    ));
                }
                let key = format!(
                    "{}\n{}\n{}\n{}",
                    entry.source_id, entry.model_id, entry.variant_id, entry.revision
                );
                if dedupe.insert(key) {
                    matches.push(M3CatalogMatch {
                        fit: evaluate_hardware_fit(&entry, &hardware)?,
                        model: entry,
                    });
                }
            }
        }
        matches.sort_by(|left, right| {
            fit_rank(&left.fit.rating)
                .cmp(&fit_rank(&right.fit.rating))
                .then_with(|| left.model.display_name.cmp(&right.model.display_name))
                .then_with(|| left.model.variant_id.cmp(&right.model.variant_id))
        });
        matches.truncate(limit);
        Ok(matches)
    }

    pub fn list_installed_models(&self) -> M3HubResult<Vec<M3InstalledModelView>> {
        let _guard = lock(&self.state_lock)?;
        let state = load_hub_state(&self.state_root, &self.models_root)?;
        state_to_views(&state, &self.models_root)
    }

    pub async fn download_model(
        &self,
        request: &M3DownloadRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledModelView> {
        context.preflight("download model")?;
        request.model.validate()?;
        validate_sha256(&request.accepted_license_sha256, "acceptedLicenseSha256")?;
        let expected_license = request.model.license.declaration_sha256();
        if !constant_time_eq(
            expected_license.as_bytes(),
            request.accepted_license_sha256.as_bytes(),
        ) {
            return Err(M3HubError::Forbidden(
                "license acceptance does not match the catalog declaration".to_string(),
            ));
        }
        let _mutation = self.mutation_lock.lock().await;
        let asset_id = request.model.asset_id();
        let asset_key = request.model.asset_key();
        let version_key = request.model.version_key();

        {
            let _guard = lock(&self.state_lock)?;
            let state = load_hub_state(&self.state_root, &self.models_root)?;
            if let Some(existing) = state.models.iter().find(|model| {
                model.asset_id == asset_id
                    && model.active_version_key == version_key
                    && model.versions.iter().any(|version| {
                        version.version_key == version_key
                            && version.model.sha256 == request.model.sha256
                    })
            }) {
                verify_stored_model(existing, &self.models_root)?;
                return stored_model_view(existing, &self.models_root);
            }
        }

        let probe = run_bounded(
            context,
            "probe model download",
            self.download.probe(&request.model.download_url, context),
        )
        .await?;
        if probe.total_bytes != request.model.size_bytes {
            return Err(invalid(
                "download.contentLength",
                format!(
                    "catalog declares {} bytes but server declares {}",
                    request.model.size_bytes, probe.total_bytes
                ),
            ));
        }
        let partial_path = self
            .downloads_root
            .join(format!("{asset_key}{DOWNLOAD_SUFFIX}"));
        let resume_path = self
            .downloads_root
            .join(format!("{asset_key}{RESUME_SUFFIX}"));
        let expected_resume = M3ResumeState {
            schema_version: M3_HUB_SCHEMA_VERSION,
            asset_key: asset_key.clone(),
            version_key: version_key.clone(),
            url: request.model.download_url.clone(),
            expected_sha256: request.model.sha256.clone(),
            total_bytes: request.model.size_bytes,
            etag: probe.etag.clone(),
        };
        prepare_resume_files(&partial_path, &resume_path, &expected_resume, &probe)?;
        atomic_write_private(&resume_path, &canonical_json(&expected_resume)?)?;
        let mut offset = regular_file_len_or_zero(&partial_path)?;
        if offset > 0 && !probe.accepts_ranges {
            remove_owned_file(&partial_path)?;
            offset = 0;
        }
        let remaining = request.model.size_bytes.saturating_sub(offset);
        let available = self.storage_status()?.available_for_models_bytes;
        if remaining > available {
            return Err(M3HubError::Storage {
                required: remaining,
                available,
            });
        }

        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut partial = options
            .open(&partial_path)
            .map_err(|source| io_at("open partial model", &partial_path, source))?;
        while offset < request.model.size_bytes {
            context.preflight("download model")?;
            let requested = usize::try_from(
                (request.model.size_bytes - offset).min(self.config.download_chunk_bytes as u64),
            )
            .map_err(|_| invalid("download.range", "size conversion overflow"))?;
            let chunk = run_bounded(
                context,
                "download model range",
                self.download.read_range(
                    &request.model.download_url,
                    offset,
                    requested,
                    probe.etag.as_deref(),
                    context,
                ),
            )
            .await?;
            validate_download_chunk(&chunk, &probe, offset, requested)?;
            partial
                .write_all(&chunk.bytes)
                .and_then(|_| partial.sync_data())
                .map_err(|source| io_at("append partial model", &partial_path, source))?;
            offset = offset
                .checked_add(chunk.bytes.len() as u64)
                .ok_or_else(|| invalid("download.offset", "overflow"))?;
        }
        drop(partial);
        let actual_digest = sha256_file(&partial_path, request.model.size_bytes)?;
        if !constant_time_eq(actual_digest.as_bytes(), request.model.sha256.as_bytes()) {
            remove_owned_file(&partial_path)?;
            remove_owned_file(&resume_path)?;
            return Err(M3HubError::Integrity {
                expected: request.model.sha256.clone(),
                actual: actual_digest,
            });
        }

        let asset_root = self.models_root.join(&asset_key);
        ensure_private_directory(&asset_root)?;
        let final_root = asset_root.join(&version_key);
        let newly_installed = if final_root.exists() {
            verify_model_directory(&final_root, &request.model)?;
            remove_owned_file(&partial_path)?;
            false
        } else {
            let staging = asset_root.join(format!(".staging-{}", Uuid::new_v4()));
            ensure_private_directory(&staging)?;
            let staging_payload = staging.join(MODEL_PAYLOAD_FILE);
            fs::rename(&partial_path, &staging_payload)
                .map_err(|source| io_at("stage verified model", &staging_payload, source))?;
            harden_file(&staging_payload)?;
            let manifest_path = staging.join(MODEL_MANIFEST_FILE);
            atomic_write_private(&manifest_path, &canonical_json(&request.model)?)?;
            sync_directory(&staging)?;
            fs::rename(&staging, &final_root)
                .map_err(|source| io_at("publish verified model", &final_root, source))?;
            sync_directory(&asset_root)?;
            true
        };
        remove_owned_file(&resume_path)?;
        let artifact_relative_path = relative_model_payload(&asset_key, &version_key);
        let installed_at_ms = self.clock.now_ms()?;

        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let index = state
            .models
            .iter()
            .position(|model| model.asset_id == asset_id);
        let stored_version = M3StoredModelVersion {
            version_key: version_key.clone(),
            model: request.model.clone(),
            artifact_relative_path,
            installed_at_ms,
        };
        match index {
            Some(index) => {
                let stored = &mut state.models[index];
                if stored.asset_key != asset_key {
                    return Err(M3HubError::State(
                        "asset id maps to an unexpected storage key".to_string(),
                    ));
                }
                if let Some(version) = stored
                    .versions
                    .iter_mut()
                    .find(|version| version.version_key == version_key)
                {
                    *version = stored_version;
                } else {
                    if stored.versions.len() >= MAX_MODEL_VERSIONS {
                        return Err(M3HubError::Conflict(format!(
                            "model {asset_id} reached the {MAX_MODEL_VERSIONS}-version limit"
                        )));
                    }
                    stored.versions.push(stored_version);
                }
                stored.active_version_key = version_key;
                stored
                    .versions
                    .sort_by(|left, right| left.version_key.cmp(&right.version_key));
            }
            None => {
                if state.models.len() >= MAX_INSTALLED_MODELS {
                    return Err(M3HubError::Conflict(format!(
                        "installed model count reached {MAX_INSTALLED_MODELS}"
                    )));
                }
                state.models.push(M3StoredModel {
                    asset_id: asset_id.clone(),
                    asset_key,
                    active_version_key: version_key,
                    versions: vec![stored_version],
                });
                state
                    .models
                    .sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
            }
        }
        let candidate_views = state_to_views(&state, &self.models_root)?;
        let reconciled = self.reconcile_candidate(&candidate_views, context).await;
        let drivers = match reconciled {
            Ok(drivers) => drivers,
            Err(error) => {
                if newly_installed {
                    let _ = remove_owned_directory(&final_root);
                }
                return Err(error);
            }
        };
        {
            let _guard = lock(&self.state_lock)?;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        if let Some(drivers) = drivers {
            *write_lock(&self.runtimes)? = drivers;
        }
        candidate_views
            .into_iter()
            .find(|model| model.asset_id == asset_id)
            .ok_or_else(|| M3HubError::State("installed model vanished from candidate".to_string()))
    }

    pub async fn update_model(
        &self,
        asset_id: &str,
        request: &M3DownloadRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledModelView> {
        validate_identifier(asset_id, "assetId")?;
        if request.model.asset_id() != asset_id {
            return Err(M3HubError::Conflict(
                "update entry does not identify the installed asset".to_string(),
            ));
        }
        if !self
            .list_installed_models()?
            .iter()
            .any(|model| model.asset_id == asset_id)
        {
            return Err(M3HubError::NotFound(asset_id.to_string()));
        }
        self.download_model(request, context).await
    }

    /// Atomically make an already verified local version active. The runtime
    /// inventory is reconciled before the durable generation is published,
    /// and a loaded model blocks the switch so no process can keep using a
    /// path whose semantic identity changed underneath it.
    pub async fn activate_model_version(
        &self,
        request: &M3ActivateModelVersionRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledModelView> {
        validate_identifier(&request.asset_id, "assetId")?;
        validate_sha256(&request.version_key, "versionKey")?;
        context.preflight("activate model version")?;
        let _mutation = self.mutation_lock.lock().await;
        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let index = state
            .models
            .iter()
            .position(|model| model.asset_id == request.asset_id)
            .ok_or_else(|| M3HubError::NotFound(request.asset_id.clone()))?;
        let stored = state.models[index].clone();
        let target = stored
            .versions
            .iter()
            .find(|version| version.version_key == request.version_key)
            .ok_or_else(|| M3HubError::NotFound(request.version_key.clone()))?;
        verify_model_directory(
            &self
                .models_root
                .join(&stored.asset_key)
                .join(&target.version_key),
            &target.model,
        )?;
        if stored.active_version_key == request.version_key {
            return stored_model_view(&stored, &self.models_root);
        }
        let active = stored
            .versions
            .iter()
            .find(|version| version.version_key == stored.active_version_key)
            .ok_or_else(|| M3HubError::State("active model version is missing".to_string()))?;
        self.ensure_model_not_running(&active.model.model_id, context)
            .await?;
        state.models[index].active_version_key = request.version_key.clone();
        let candidate_views = state_to_views(&state, &self.models_root)?;
        let drivers = self.reconcile_candidate(&candidate_views, context).await?;
        {
            let _guard = lock(&self.state_lock)?;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        if let Some(drivers) = drivers {
            *write_lock(&self.runtimes)? = drivers;
        }
        candidate_views
            .into_iter()
            .find(|model| model.asset_id == request.asset_id)
            .ok_or_else(|| M3HubError::State("activated model vanished".to_string()))
    }

    /// Remove every inactive model version after an exact, asset-bound
    /// confirmation. Directories are first isolated into an owned trash tree;
    /// any reconciliation or state-publication failure restores them before
    /// returning, so cleanup never leaves a partially pruned inventory.
    pub async fn prune_model_versions(
        &self,
        request: &M3PruneModelVersionsRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledModelView> {
        validate_identifier(&request.asset_id, "assetId")?;
        if request.confirmation != format!("PRUNE {}", request.asset_id) {
            return Err(M3HubError::Forbidden(
                "version cleanup requires exact confirmation".to_string(),
            ));
        }
        context.preflight("prune model versions")?;
        let _mutation = self.mutation_lock.lock().await;
        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let index = state
            .models
            .iter()
            .position(|model| model.asset_id == request.asset_id)
            .ok_or_else(|| M3HubError::NotFound(request.asset_id.clone()))?;
        let stored = state.models[index].clone();
        if stored.versions.len() == 1 {
            return stored_model_view(&stored, &self.models_root);
        }
        let asset_root = self.models_root.join(&stored.asset_key);
        verify_asset_root(&asset_root, &stored)?;
        let trash = self
            .models_root
            .join(format!(".trash-prune-{}", Uuid::new_v4()));
        ensure_private_directory(&trash)?;
        let mut isolated = Vec::new();
        for version in stored
            .versions
            .iter()
            .filter(|version| version.version_key != stored.active_version_key)
        {
            let source = asset_root.join(&version.version_key);
            let destination = trash.join(&version.version_key);
            if let Err(source_error) = fs::rename(&source, &destination) {
                restore_isolated_versions(&isolated, &asset_root);
                let _ = fs::remove_dir(&trash);
                return Err(io_at(
                    "isolate inactive model version",
                    &source,
                    source_error,
                ));
            }
            isolated.push((destination, source));
        }
        state.models[index]
            .versions
            .retain(|version| version.version_key == stored.active_version_key);
        let candidate_views = match state_to_views(&state, &self.models_root) {
            Ok(views) => views,
            Err(error) => {
                restore_isolated_versions(&isolated, &asset_root);
                let _ = fs::remove_dir(&trash);
                return Err(error);
            }
        };
        let drivers = match self.reconcile_candidate(&candidate_views, context).await {
            Ok(drivers) => drivers,
            Err(error) => {
                restore_isolated_versions(&isolated, &asset_root);
                let _ = fs::remove_dir(&trash);
                return Err(error);
            }
        };
        let saved = {
            let _guard = lock(&self.state_lock)?;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)
        };
        if let Err(error) = saved {
            restore_isolated_versions(&isolated, &asset_root);
            let _ = fs::remove_dir(&trash);
            return Err(error);
        }
        if let Some(drivers) = drivers {
            *write_lock(&self.runtimes)? = drivers;
        }
        remove_owned_directory(&trash)?;
        sync_directory(&asset_root)?;
        sync_directory(&self.models_root)?;
        candidate_views
            .into_iter()
            .find(|model| model.asset_id == request.asset_id)
            .ok_or_else(|| M3HubError::State("pruned model vanished".to_string()))
    }

    pub async fn delete_model(
        &self,
        request: &M3DeleteModelRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<bool> {
        validate_identifier(&request.asset_id, "assetId")?;
        if request.confirmation != format!("DELETE {}", request.asset_id) {
            return Err(M3HubError::Forbidden(
                "model deletion requires exact confirmation".to_string(),
            ));
        }
        context.preflight("delete model")?;
        let _mutation = self.mutation_lock.lock().await;
        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let Some(index) = state
            .models
            .iter()
            .position(|model| model.asset_id == request.asset_id)
        else {
            return Ok(false);
        };
        let stored = state.models[index].clone();
        let active_model = stored
            .versions
            .iter()
            .find(|version| version.version_key == stored.active_version_key)
            .ok_or_else(|| M3HubError::State("active model version is missing".to_string()))?;
        self.ensure_model_not_running(&active_model.model.model_id, context)
            .await?;
        let asset_root = self.models_root.join(&stored.asset_key);
        verify_asset_root(&asset_root, &stored)?;
        let trash = self.models_root.join(format!(".trash-{}", Uuid::new_v4()));
        fs::rename(&asset_root, &trash)
            .map_err(|source| io_at("isolate deleted model", &asset_root, source))?;
        state.models.remove(index);
        let candidate_views = state_to_views(&state, &self.models_root)?;
        let reconciled = self.reconcile_candidate(&candidate_views, context).await;
        let drivers = match reconciled {
            Ok(drivers) => drivers,
            Err(error) => {
                let _ = fs::rename(&trash, &asset_root);
                return Err(error);
            }
        };
        let saved = {
            let _guard = lock(&self.state_lock)?;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)
        };
        if let Err(error) = saved {
            let _ = fs::rename(&trash, &asset_root);
            return Err(error);
        }
        if let Some(drivers) = drivers {
            *write_lock(&self.runtimes)? = drivers;
        }
        remove_owned_directory(&trash)?;
        sync_directory(&self.models_root)?;
        Ok(true)
    }

    pub fn list_runtimes(&self) -> M3HubResult<Vec<M3RuntimeCapabilityView>> {
        Ok(read_lock(&self.runtimes)?
            .values()
            .map(|runtime| runtime.capabilities())
            .collect())
    }

    pub async fn refresh_runtimes(
        &self,
        context: &M3OperationContext,
    ) -> M3HubResult<Vec<M3RuntimeCapabilityView>> {
        context.preflight("refresh runtime factories")?;
        let _mutation = self.mutation_lock.lock().await;
        let installed = self.list_installed_models()?;
        if let Some(drivers) = self.reconcile_candidate(&installed, context).await? {
            *write_lock(&self.runtimes)? = drivers;
        }
        self.list_runtimes()
    }

    pub async fn runtime_status(
        &self,
        runtime_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<M3RuntimeStatusView> {
        self.runtime(runtime_id)?.status(context).await
    }

    pub async fn runtime_inventory(
        &self,
        runtime_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<RuntimeInventory> {
        self.runtime(runtime_id)?.inventory(context).await
    }

    pub async fn load_model(
        &self,
        request: &M3LoadModelRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        validate_identifier(&request.runtime_id, "runtimeId")?;
        validate_identifier(&request.asset_id, "assetId")?;
        let runtime = self.runtime(&request.runtime_id)?;
        let model = self.resolve_installed_model(&request.asset_id)?;
        if runtime.descriptor().kind != model.runtime {
            return Err(M3HubError::Conflict(
                "selected runtime cannot load this model format".to_string(),
            ));
        }
        let settings = self
            .runtime_config(&request.runtime_id)?
            .unwrap_or_default();
        runtime
            .load(
                &model,
                &settings,
                request.keep_alive,
                request.replace_existing,
                context,
            )
            .await
    }

    pub async fn unload_model(
        &self,
        request: &M3UnloadModelRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        validate_identifier(&request.runtime_id, "runtimeId")?;
        validate_identifier(&request.model_id, "modelId")?;
        self.runtime(&request.runtime_id)?
            .unload(&request.model_id, request.force_exact_owner, context)
            .await
    }

    pub async fn runtime_logs(
        &self,
        runtime_id: &str,
        max_bytes: usize,
        context: &M3OperationContext,
    ) -> M3HubResult<RuntimeLogTail> {
        if max_bytes == 0 || max_bytes > 16 * 1024 * 1024 {
            return Err(invalid("maxBytes", "must be between 1 and 16777216"));
        }
        self.runtime(runtime_id)?.logs(max_bytes, context).await
    }

    pub async fn runtime_metrics(
        &self,
        runtime_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<M3RuntimeMetricsView> {
        self.runtime(runtime_id)?.metrics(context).await
    }

    pub fn set_runtime_config(
        &self,
        request: &M3SetRuntimeConfigRequest,
    ) -> M3HubResult<BTreeMap<String, SettingValue>> {
        validate_identifier(&request.runtime_id, "runtimeId")?;
        self.runtime(&request.runtime_id)?
            .validate_config(&request.values)?;
        let _guard = lock(&self.state_lock)?;
        let mut state = load_hub_state(&self.state_root, &self.models_root)?;
        state
            .runtime_configs
            .insert(request.runtime_id.clone(), request.values.clone());
        save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        Ok(request.values.clone())
    }

    pub fn runtime_config(
        &self,
        runtime_id: &str,
    ) -> M3HubResult<Option<BTreeMap<String, SettingValue>>> {
        validate_identifier(runtime_id, "runtimeId")?;
        let _guard = lock(&self.state_lock)?;
        Ok(load_hub_state(&self.state_root, &self.models_root)?
            .runtime_configs
            .get(runtime_id)
            .cloned())
    }

    pub fn lan_policy(&self) -> M3HubResult<Option<LanServerPolicy>> {
        let _guard = lock(&self.state_lock)?;
        Ok(load_hub_state(&self.state_root, &self.models_root)?.lan_policy)
    }

    pub fn validate_lan_policy(policy: &LanServerPolicy) -> M3HubResult<()> {
        policy.validate().map_err(M3HubError::from)
    }

    pub fn configure_lan(&self, policy: LanServerPolicy) -> M3HubResult<LanServerPolicy> {
        policy.validate().map_err(M3HubError::from)?;
        let factory = self.lan_factory.as_ref().ok_or_else(|| {
            M3HubError::Unsupported("LAN security integration is unavailable".to_string())
        })?;
        let previous_policy = self.lan_policy()?;
        let previous_controller = read_lock(&self.lan)?.clone();
        if previous_policy
            .as_ref()
            .is_some_and(|current| current != &policy)
        {
            if let Some(controller) = previous_controller {
                controller
                    .revoke_all_tokens(self.clock.now_ms()?, "127.0.0.1")
                    .map_err(M3HubError::from)?;
            }
        }
        let controller = factory.create(&self.lan_state_root, policy.clone())?;
        {
            let _guard = lock(&self.state_lock)?;
            let mut state = load_hub_state(&self.state_root, &self.models_root)?;
            state.lan_policy = Some(policy.clone());
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        *write_lock(&self.lan)? = Some(controller);
        Ok(policy)
    }

    pub fn disable_lan(&self, confirmation: &str) -> M3HubResult<bool> {
        if confirmation != "DISABLE LAN API" {
            return Err(M3HubError::Forbidden(
                "disabling LAN requires exact confirmation".to_string(),
            ));
        }
        let controller = read_lock(&self.lan)?.clone();
        let existed = controller.is_some();
        if let Some(controller) = controller {
            controller
                .revoke_all_tokens(self.clock.now_ms()?, "127.0.0.1")
                .map_err(M3HubError::from)?;
        }
        {
            let _guard = lock(&self.state_lock)?;
            let mut state = load_hub_state(&self.state_root, &self.models_root)?;
            state.lan_policy = None;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        *write_lock(&self.lan)? = None;
        Ok(existed)
    }

    pub fn begin_pairing(
        &self,
        request: PairingRequest,
        now_ms: u64,
        remote_address: &str,
    ) -> M3HubResult<PairingChallengeView> {
        self.lan_controller()?
            .begin_pairing(request, now_ms, remote_address)
            .map_err(M3HubError::from)
    }

    pub fn complete_pairing(
        &self,
        challenge_id: &str,
        pairing_code: &str,
        now_ms: u64,
        remote_address: &str,
    ) -> M3HubResult<PairedToken> {
        self.lan_controller()?
            .complete_pairing(challenge_id, pairing_code, now_ms, remote_address)
            .map_err(M3HubError::from)
    }

    pub fn revoke_token(
        &self,
        token_id: &str,
        now_ms: u64,
        remote_address: &str,
    ) -> M3HubResult<ScopedTokenView> {
        self.lan_controller()?
            .revoke_token(token_id, now_ms, remote_address)
            .map_err(M3HubError::from)
    }

    pub fn list_tokens(&self) -> M3HubResult<Vec<ScopedTokenView>> {
        self.lan_controller()?
            .list_tokens()
            .map_err(M3HubError::from)
    }

    pub fn security_audit_events(&self) -> M3HubResult<Vec<SecurityAuditEvent>> {
        self.lan_controller()?
            .audit_events()
            .map_err(M3HubError::from)
    }

    /// Applies the same scoped-token, backend, model, mutation, rate-limit,
    /// and destructive-confirmation policy used by compatibility inference to
    /// an HTTP listener's model/runtime lifecycle routes.
    pub fn authorize_external_operation(
        &self,
        request: &M3ExternalOperationAuthorization,
    ) -> M3HubResult<AuthorizedToken> {
        self.lan_controller()?
            .authorize(&AuthorizationRequest {
                bearer_token: request.bearer_token.clone(),
                scope: request.scope,
                backend: request.backend,
                model_id: request.model_id.clone(),
                input_bytes: request.input_bytes,
                remote_address: request.remote_address.clone(),
                destructive_confirmation: request.destructive_confirmation.clone(),
                now_ms: request.now_ms,
            })
            .map_err(M3HubError::from)
    }

    async fn reconcile_candidate(
        &self,
        views: &[M3InstalledModelView],
        context: &M3OperationContext,
    ) -> M3HubResult<Option<BTreeMap<String, Arc<dyn M3RuntimeDriver>>>> {
        let Some(reconciler) = &self.runtime_reconciler else {
            return Ok(None);
        };
        let drivers = run_bounded(
            context,
            "reconcile runtime inventory",
            reconciler.reconcile(views, context),
        )
        .await?;
        runtime_map(drivers).map(Some)
    }

    async fn ensure_model_not_running(
        &self,
        model_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let runtimes = read_lock(&self.runtimes)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in runtimes {
            let status = runtime.status(context).await?;
            let running = match status {
                M3RuntimeStatusView::Adapter { running_models, .. } => running_models
                    .iter()
                    .any(|running| running.model_id == model_id),
                M3RuntimeStatusView::Mlx { status } => {
                    matches!(status, MlxRuntimeStatus::Running { handle, .. } if handle.model_id == model_id)
                }
            };
            if running {
                return Err(M3HubError::Conflict(format!(
                    "model {model_id} is still loaded in runtime {}",
                    runtime.descriptor().runtime_id
                )));
            }
        }
        Ok(())
    }

    fn runtime(&self, runtime_id: &str) -> M3HubResult<Arc<dyn M3RuntimeDriver>> {
        validate_identifier(runtime_id, "runtimeId")?;
        read_lock(&self.runtimes)?
            .get(runtime_id)
            .cloned()
            .ok_or_else(|| M3HubError::NotFound(format!("runtime {runtime_id}")))
    }

    fn resolve_installed_model(&self, asset_id: &str) -> M3HubResult<M3ResolvedModel> {
        let _guard = lock(&self.state_lock)?;
        let state = load_hub_state(&self.state_root, &self.models_root)?;
        let stored = state
            .models
            .iter()
            .find(|model| model.asset_id == asset_id)
            .ok_or_else(|| M3HubError::NotFound(format!("model {asset_id}")))?;
        verify_stored_model(stored, &self.models_root)?;
        let active = stored
            .versions
            .iter()
            .find(|version| version.version_key == stored.active_version_key)
            .ok_or_else(|| M3HubError::State("active model version is missing".to_string()))?;
        Ok(M3ResolvedModel {
            asset_id: stored.asset_id.clone(),
            model_id: active.model.model_id.clone(),
            runtime: active.model.runtime,
            artifact_path: self.models_root.join(&active.artifact_relative_path),
            size_bytes: active.model.size_bytes,
            capabilities: active.model.capabilities.clone(),
        })
    }

    fn lan_controller(&self) -> M3HubResult<Arc<LanAccessController>> {
        read_lock(&self.lan)?
            .clone()
            .ok_or_else(|| M3HubError::Unsupported("LAN API is disabled".to_string()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum M3ApiCaller {
    Internal,
    External {
        bearer_token: String,
        remote_address: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ExternalOperationAuthorization {
    pub bearer_token: String,
    pub scope: ApiScope,
    pub backend: ApiBackend,
    pub model_id: Option<String>,
    pub input_bytes: u64,
    pub remote_address: String,
    pub destructive_confirmation: Option<String>,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ApiDispatchRequest {
    pub protocol: CompatibilityProtocol,
    pub runtime_id: String,
    pub request_id: String,
    pub body: Vec<u8>,
    pub caller: M3ApiCaller,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ApiDispatchResponse {
    pub status: u16,
    pub body: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CancelInferenceRequest {
    pub protocol: CompatibilityProtocol,
    pub runtime_id: String,
    pub request_id: String,
    pub model_id: String,
    pub caller: M3ApiCaller,
    pub now_ms: u64,
}

impl M3RuntimeHub {
    pub async fn dispatch_api(
        &self,
        request: &M3ApiDispatchRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3ApiDispatchResponse> {
        context.preflight("dispatch compatibility request")?;
        let canonical = crate::compatibility_hub::translate_request(
            request.protocol,
            &request.request_id,
            &request.body,
        )?;
        if canonical.stream {
            return Err(M3HubError::Conflict(
                "streaming requests must use dispatch_api_stream".to_string(),
            ));
        }
        let runtime = self.runtime(&request.runtime_id)?;
        self.authorize_api(
            &request.caller,
            request.protocol,
            &runtime.descriptor(),
            &canonical.model,
            request.body.len() as u64,
            request.now_ms,
        )?;
        let response = runtime.complete(&canonical, context).await?;
        if response.model != canonical.model {
            return Err(M3HubError::Runtime(
                "inference driver returned a response for another model".to_string(),
            ));
        }
        Ok(M3ApiDispatchResponse {
            status: 200,
            body: encode_response(request.protocol, &response)?,
        })
    }

    pub async fn dispatch_api_stream(
        &self,
        request: &M3ApiDispatchRequest,
        sink: &mut dyn M3ProtocolFrameSink,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        context.preflight("dispatch compatibility stream")?;
        let canonical = crate::compatibility_hub::translate_request(
            request.protocol,
            &request.request_id,
            &request.body,
        )?;
        if !canonical.stream {
            return Err(M3HubError::Conflict(
                "non-streaming requests must use dispatch_api".to_string(),
            ));
        }
        let runtime = self.runtime(&request.runtime_id)?;
        self.authorize_api(
            &request.caller,
            request.protocol,
            &runtime.descriptor(),
            &canonical.model,
            request.body.len() as u64,
            request.now_ms,
        )?;
        let mut encoding = ProtocolEncodingSink {
            protocol: request.protocol,
            downstream: sink,
        };
        runtime.stream(&canonical, &mut encoding, context).await
    }

    pub async fn cancel_inference(
        &self,
        request: &M3CancelInferenceRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<bool> {
        validate_identifier(&request.request_id, "requestId")?;
        validate_identifier(&request.model_id, "modelId")?;
        let runtime = self.runtime(&request.runtime_id)?;
        self.authorize_api(
            &request.caller,
            request.protocol,
            &runtime.descriptor(),
            &request.model_id,
            0,
            request.now_ms,
        )?;
        runtime.cancel(&request.request_id, context).await
    }

    fn authorize_api(
        &self,
        caller: &M3ApiCaller,
        protocol: CompatibilityProtocol,
        runtime: &M3RuntimeDescriptor,
        model_id: &str,
        input_bytes: u64,
        now_ms: u64,
    ) -> M3HubResult<()> {
        match caller {
            M3ApiCaller::Internal => Ok(()),
            M3ApiCaller::External {
                bearer_token,
                remote_address,
            } => {
                self.lan_controller()?.authorize(&AuthorizationRequest {
                    bearer_token: bearer_token.clone(),
                    scope: protocol_scope(protocol),
                    backend: runtime.api_backend,
                    model_id: Some(model_id.to_string()),
                    input_bytes,
                    remote_address: remote_address.clone(),
                    destructive_confirmation: None,
                    now_ms,
                })?;
                Ok(())
            }
        }
    }
}

struct ProtocolEncodingSink<'a> {
    protocol: CompatibilityProtocol,
    downstream: &'a mut dyn M3ProtocolFrameSink,
}

impl M3CanonicalStreamSink for ProtocolEncodingSink<'_> {
    fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
        let frames =
            encode_stream_event(self.protocol, &event).map_err(|error| error.to_string())?;
        for frame in frames {
            self.downstream.emit(frame)?;
        }
        Ok(())
    }
}

struct MlxCanonicalSink<'a> {
    downstream: &'a mut dyn M3CanonicalStreamSink,
    response_id: String,
    text_index: Option<usize>,
    text_ended: bool,
    tool_indices: BTreeMap<String, usize>,
    had_tool_calls: bool,
    next_index: usize,
    completed: bool,
}

impl<'a> MlxCanonicalSink<'a> {
    fn new(downstream: &'a mut dyn M3CanonicalStreamSink, response_id: String) -> Self {
        Self {
            downstream,
            response_id,
            text_index: None,
            text_ended: false,
            tool_indices: BTreeMap::new(),
            had_tool_calls: false,
            next_index: 0,
            completed: false,
        }
    }

    fn end_text(&mut self) -> Result<(), String> {
        if let Some(index) = self.text_index {
            if !self.text_ended {
                self.downstream
                    .emit(CanonicalStreamEvent::TextEnd { index })?;
                self.text_ended = true;
            }
        }
        Ok(())
    }

    fn finish_if_needed(&mut self, summary: &MlxGenerationSummary) -> M3HubResult<()> {
        if self.completed {
            return Ok(());
        }
        self.end_text().map_err(stream_sink_error)?;
        self.downstream
            .emit(CanonicalStreamEvent::ResponseCompleted {
                response_id: self.response_id.clone(),
                finish_reason: if self.had_tool_calls {
                    "tool_use".to_string()
                } else {
                    summary.finish_reason.clone()
                },
                usage: CanonicalUsage {
                    input_tokens: summary.input_tokens,
                    output_tokens: summary.output_tokens,
                },
            })
            .map_err(stream_sink_error)?;
        self.completed = true;
        Ok(())
    }
}

impl MlxStreamSink for MlxCanonicalSink<'_> {
    fn emit(&mut self, event: MlxStreamEvent) -> Result<(), String> {
        match event {
            MlxStreamEvent::Started { .. } => Ok(()),
            MlxStreamEvent::TextDelta { text } => {
                let index = match self.text_index {
                    Some(index) => index,
                    None => {
                        let index = self.next_index;
                        self.next_index = self.next_index.saturating_add(1);
                        self.text_index = Some(index);
                        self.downstream
                            .emit(CanonicalStreamEvent::TextStart { index })?;
                        index
                    }
                };
                if self.text_ended {
                    return Err("MLX emitted text after ending the text block".to_string());
                }
                self.downstream
                    .emit(CanonicalStreamEvent::TextDelta { index, text })
            }
            MlxStreamEvent::ToolCallStart { call_id, name } => {
                self.end_text()?;
                if self.tool_indices.contains_key(&call_id) {
                    return Err("MLX emitted a duplicate tool call id".to_string());
                }
                let index = self.next_index;
                self.next_index = self.next_index.saturating_add(1);
                self.tool_indices.insert(call_id.clone(), index);
                self.had_tool_calls = true;
                self.downstream.emit(CanonicalStreamEvent::ToolCallStart {
                    index,
                    call_id,
                    name,
                })
            }
            MlxStreamEvent::ToolCallArgumentsDelta { call_id, json } => {
                let index = *self
                    .tool_indices
                    .get(&call_id)
                    .ok_or_else(|| "MLX emitted arguments before tool start".to_string())?;
                self.downstream
                    .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                        index,
                        call_id,
                        json_delta: json,
                    })
            }
            MlxStreamEvent::ToolCallEnd { call_id } => {
                let index = self
                    .tool_indices
                    .remove(&call_id)
                    .ok_or_else(|| "MLX ended an unknown tool call".to_string())?;
                self.downstream
                    .emit(CanonicalStreamEvent::ToolCallEnd { index, call_id })
            }
            MlxStreamEvent::Completed {
                input_tokens,
                output_tokens,
            } => {
                if !self.tool_indices.is_empty() {
                    return Err("MLX completed with unfinished tool calls".to_string());
                }
                self.end_text()?;
                self.downstream
                    .emit(CanonicalStreamEvent::ResponseCompleted {
                        response_id: self.response_id.clone(),
                        finish_reason: if self.had_tool_calls {
                            "tool_use".to_string()
                        } else {
                            "stop".to_string()
                        },
                        usage: CanonicalUsage {
                            input_tokens,
                            output_tokens,
                        },
                    })?;
                self.completed = true;
                Ok(())
            }
            MlxStreamEvent::Error { code, message } => {
                self.downstream.emit(CanonicalStreamEvent::Error {
                    code,
                    message,
                    retryable: false,
                })
            }
        }
    }
}

#[derive(Default)]
struct CanonicalCollector {
    response_id: Option<String>,
    model: Option<String>,
    created_at_seconds: Option<u64>,
    texts: BTreeMap<usize, String>,
    tools: BTreeMap<usize, CanonicalToolAccumulator>,
    usage: Option<CanonicalUsage>,
    finish_reason: Option<String>,
    error: Option<String>,
}

struct CanonicalToolAccumulator {
    call_id: String,
    name: String,
    arguments: String,
    ended: bool,
}

impl M3CanonicalStreamSink for CanonicalCollector {
    fn emit(&mut self, event: CanonicalStreamEvent) -> Result<(), String> {
        match event {
            CanonicalStreamEvent::ResponseStart {
                response_id,
                model,
                created_at_seconds,
            } => {
                if self.response_id.replace(response_id).is_some() {
                    return Err("duplicate response start".to_string());
                }
                self.model = Some(model);
                self.created_at_seconds = Some(created_at_seconds);
            }
            CanonicalStreamEvent::TextStart { index } => {
                if self.texts.insert(index, String::new()).is_some() {
                    return Err("duplicate text block".to_string());
                }
            }
            CanonicalStreamEvent::TextDelta { index, text } => self
                .texts
                .get_mut(&index)
                .ok_or_else(|| "text delta before text start".to_string())?
                .push_str(&text),
            CanonicalStreamEvent::TextEnd { index } => {
                if !self.texts.contains_key(&index) {
                    return Err("text end before text start".to_string());
                }
            }
            CanonicalStreamEvent::ToolCallStart {
                index,
                call_id,
                name,
            } => {
                if self
                    .tools
                    .insert(
                        index,
                        CanonicalToolAccumulator {
                            call_id,
                            name,
                            arguments: String::new(),
                            ended: false,
                        },
                    )
                    .is_some()
                {
                    return Err("duplicate tool block".to_string());
                }
            }
            CanonicalStreamEvent::ToolCallArgumentsDelta {
                index,
                call_id,
                json_delta,
            } => {
                let tool = self
                    .tools
                    .get_mut(&index)
                    .ok_or_else(|| "tool arguments before tool start".to_string())?;
                if tool.call_id != call_id || tool.ended {
                    return Err("tool argument call id/order mismatch".to_string());
                }
                tool.arguments.push_str(&json_delta);
            }
            CanonicalStreamEvent::ToolCallEnd { index, call_id } => {
                let tool = self
                    .tools
                    .get_mut(&index)
                    .ok_or_else(|| "tool end before tool start".to_string())?;
                if tool.call_id != call_id || tool.ended {
                    return Err("tool end call id/order mismatch".to_string());
                }
                tool.ended = true;
            }
            CanonicalStreamEvent::ResponseCompleted {
                response_id,
                finish_reason,
                usage,
            } => {
                if self.response_id.as_deref() != Some(&response_id) {
                    return Err("response completion id mismatch".to_string());
                }
                self.finish_reason = Some(finish_reason);
                self.usage = Some(usage);
            }
            CanonicalStreamEvent::Error { code, message, .. } => {
                self.error = Some(format!("{code}: {message}"));
            }
        }
        Ok(())
    }
}

impl CanonicalCollector {
    fn into_response(
        self,
        request: &CanonicalInferenceRequest,
        fallback_created_at_seconds: u64,
    ) -> M3HubResult<CanonicalInferenceResponse> {
        if let Some(error) = self.error {
            return Err(M3HubError::Runtime(error));
        }
        let mut ordered = BTreeMap::<usize, CanonicalContent>::new();
        for (index, text) in self.texts {
            ordered.insert(index, CanonicalContent::Text { text });
        }
        for (index, tool) in self.tools {
            if !tool.ended {
                return Err(M3HubError::Runtime(
                    "stream ended with unfinished tool call".to_string(),
                ));
            }
            let input: Value = serde_json::from_str(&tool.arguments).map_err(|error| {
                M3HubError::Runtime(format!("tool arguments are not valid JSON: {error}"))
            })?;
            if !input.is_object() {
                return Err(M3HubError::Runtime(
                    "tool arguments must decode to an object".to_string(),
                ));
            }
            if ordered
                .insert(
                    index,
                    CanonicalContent::ToolUse {
                        id: tool.call_id,
                        name: tool.name,
                        input,
                    },
                )
                .is_some()
            {
                return Err(M3HubError::Runtime(
                    "stream reused a content index".to_string(),
                ));
            }
        }
        let content = ordered.into_values().collect::<Vec<_>>();
        if content.is_empty() {
            return Err(M3HubError::Runtime(
                "stream produced no response content".to_string(),
            ));
        }
        Ok(CanonicalInferenceResponse {
            response_id: self
                .response_id
                .ok_or_else(|| M3HubError::Runtime("missing response start".to_string()))?,
            model: self.model.unwrap_or_else(|| request.model.clone()),
            content,
            finish_reason: self
                .finish_reason
                .ok_or_else(|| M3HubError::Runtime("missing response completion".to_string()))?,
            usage: self
                .usage
                .ok_or_else(|| M3HubError::Runtime("missing response usage".to_string()))?,
            created_at_seconds: self
                .created_at_seconds
                .unwrap_or(fallback_created_at_seconds),
        })
    }
}

fn canonical_message_to_mlx(message: &CanonicalMessage) -> M3HubResult<MlxMessage> {
    let role = match message.role {
        CanonicalRole::System => "system",
        CanonicalRole::User => "user",
        CanonicalRole::Assistant => "assistant",
        CanonicalRole::Tool => "tool",
    };
    let mut parts = Vec::new();
    for content in &message.content {
        match content {
            CanonicalContent::Text { text } => parts.push(text.clone()),
            CanonicalContent::ToolUse { id, name, input } => {
                parts.push(canonical_json_string(&json!({
                    "type":"tool_use","id":id,"name":name,"input":input
                }))?)
            }
            CanonicalContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => parts.push(canonical_json_string(&json!({
                "type":"tool_result","tool_use_id":tool_use_id,"content":content,"is_error":is_error
            }))?),
        }
    }
    Ok(MlxMessage {
        role: role.to_string(),
        text: parts.join("\n"),
    })
}

fn state_to_views(
    state: &M3HubState,
    models_root: &Path,
) -> M3HubResult<Vec<M3InstalledModelView>> {
    let mut output = state
        .models
        .iter()
        .map(|model| stored_model_view(model, models_root))
        .collect::<M3HubResult<Vec<_>>>()?;
    output.sort_by(|left, right| left.asset_id.cmp(&right.asset_id));
    Ok(output)
}

fn stored_model_view(
    stored: &M3StoredModel,
    models_root: &Path,
) -> M3HubResult<M3InstalledModelView> {
    let active = stored
        .versions
        .iter()
        .find(|version| version.version_key == stored.active_version_key)
        .ok_or_else(|| M3HubError::State("active version is missing".to_string()))?;
    let mut versions = stored
        .versions
        .iter()
        .map(|version| {
            let artifact_path = models_root.join(&version.artifact_relative_path);
            ensure_descendant(models_root, &artifact_path)?;
            Ok(M3InstalledVersionView {
                version_key: version.version_key.clone(),
                revision: version.model.revision.clone(),
                sha256: version.model.sha256.clone(),
                size_bytes: version.model.size_bytes,
                artifact_path,
                installed_at_ms: version.installed_at_ms,
                active: version.version_key == stored.active_version_key,
                license: version.model.license.clone(),
            })
        })
        .collect::<M3HubResult<Vec<_>>>()?;
    versions.sort_by(|left, right| left.version_key.cmp(&right.version_key));
    Ok(M3InstalledModelView {
        asset_id: stored.asset_id.clone(),
        model_id: active.model.model_id.clone(),
        display_name: active.model.display_name.clone(),
        runtime: active.model.runtime,
        variant_id: active.model.variant_id.clone(),
        capabilities: active.model.capabilities.clone(),
        estimated_ram_bytes: active.model.estimated_ram_bytes,
        estimated_vram_bytes: active.model.estimated_vram_bytes,
        required_accelerator: active.model.required_accelerator.clone(),
        active_version_key: stored.active_version_key.clone(),
        versions,
    })
}

fn runtime_map(
    runtimes: Vec<Arc<dyn M3RuntimeDriver>>,
) -> M3HubResult<BTreeMap<String, Arc<dyn M3RuntimeDriver>>> {
    if runtimes.len() > MAX_RUNTIME_COUNT {
        return Err(invalid(
            "runtimes",
            format!("at most {MAX_RUNTIME_COUNT} runtimes are accepted"),
        ));
    }
    let mut output = BTreeMap::new();
    for runtime in runtimes {
        let descriptor = runtime.descriptor();
        validate_identifier(&descriptor.runtime_id, "runtime.runtimeId")?;
        validate_text(&descriptor.label, "runtime.label", 16 * 1024)?;
        if descriptor.api_backend != descriptor.kind.api_backend() {
            return Err(invalid(
                "runtime.apiBackend",
                "must match the local runtime kind",
            ));
        }
        if output
            .insert(descriptor.runtime_id.clone(), runtime)
            .is_some()
        {
            return Err(invalid("runtimes", "runtime ids must be unique"));
        }
    }
    Ok(output)
}

fn validate_catalog_sources(sources: &[Arc<dyn M3CatalogSource>]) -> M3HubResult<()> {
    if sources.len() > 128 {
        return Err(invalid("catalogs", "at most 128 sources are accepted"));
    }
    let mut source_ids = BTreeSet::new();
    for source in sources {
        validate_identifier(source.source_id(), "catalog.sourceId")?;
        if !source_ids.insert(source.source_id().to_string()) {
            return Err(invalid("catalogs", "source ids must be unique"));
        }
    }
    Ok(())
}

fn protocol_scope(protocol: CompatibilityProtocol) -> ApiScope {
    match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => ApiScope::ChatCompletions,
        CompatibilityProtocol::OpenAiResponses => ApiScope::Responses,
        CompatibilityProtocol::AnthropicMessages => ApiScope::Messages,
    }
}

fn evaluate_hardware_fit(
    model: &M3CatalogModel,
    hardware: &HardwareSnapshot,
) -> M3HubResult<M3HardwareFit> {
    model.validate()?;
    let profile = hardware.profile().map_err(runtime_error)?;
    let mut reasons = Vec::new();
    let os_supported = model.supported_os.contains(&hardware.platform.os);
    let arch_supported = model.supported_arch.contains(&hardware.platform.arch);
    if !os_supported {
        reasons.push(format!("OS {} is not advertised", hardware.platform.os));
    }
    if !arch_supported {
        reasons.push(format!(
            "architecture {} is not advertised",
            hardware.platform.arch
        ));
    }
    let accelerator = model
        .required_accelerator
        .as_deref()
        .map(parse_accelerator)
        .transpose()?;
    let accelerator_supported =
        accelerator.is_none_or(|required| hardware.platform.supports_accelerator(required));
    if let Some(required) = accelerator {
        if !accelerator_supported {
            reasons.push(format!("required accelerator {required:?} is unavailable"));
        }
    }
    let mlx_supported = model.runtime != M3RuntimeKind::Mlx
        || (hardware.platform.os == "macos"
            && hardware.platform.arch == "aarch64"
            && hardware
                .platform
                .supports_accelerator(crate::runtime_adapter::AcceleratorKind::Metal));
    if !mlx_supported {
        reasons.push("MLX requires Apple Silicon with Metal".to_string());
    }
    let available_ram_bytes = hardware
        .available_ram_bytes
        .saturating_sub(profile.recommended_ram_reserve_bytes);
    let available_vram_bytes = accelerator
        .and_then(|required| {
            hardware
                .platform
                .accelerators
                .iter()
                .find(|entry| entry.kind == required && entry.available)
                .and_then(|entry| entry.available_memory_bytes.or(entry.total_memory_bytes))
        })
        .unwrap_or_else(|| {
            if accelerator == Some(crate::runtime_adapter::AcceleratorKind::Metal) {
                available_ram_bytes
            } else {
                0
            }
        });
    let compatible = os_supported && arch_supported && accelerator_supported && mlx_supported;
    let fits = model.estimated_ram_bytes <= available_ram_bytes
        && model.estimated_vram_bytes <= available_vram_bytes;
    let comfortably_fits = model.estimated_ram_bytes
        <= available_ram_bytes.saturating_mul(85) / 100
        && (model.estimated_vram_bytes == 0
            || model.estimated_vram_bytes <= available_vram_bytes.saturating_mul(85) / 100);
    let rating = if !compatible {
        M3HardwareFitRating::Incompatible
    } else if !fits {
        reasons.push("estimated peak memory exceeds the available budget".to_string());
        M3HardwareFitRating::TooLarge
    } else if comfortably_fits {
        M3HardwareFitRating::Recommended
    } else {
        reasons.push("estimated peak memory leaves less than 15% headroom".to_string());
        M3HardwareFitRating::Tight
    };
    Ok(M3HardwareFit {
        rating,
        required_ram_bytes: model.estimated_ram_bytes,
        available_ram_bytes,
        required_vram_bytes: model.estimated_vram_bytes,
        available_vram_bytes,
        reasons,
    })
}

fn fit_rank(rating: &M3HardwareFitRating) -> u8 {
    match rating {
        M3HardwareFitRating::Recommended => 0,
        M3HardwareFitRating::Tight => 1,
        M3HardwareFitRating::TooLarge => 2,
        M3HardwareFitRating::Incompatible => 3,
    }
}

fn parse_accelerator(value: &str) -> M3HubResult<crate::runtime_adapter::AcceleratorKind> {
    match value {
        "cpu" => Ok(crate::runtime_adapter::AcceleratorKind::Cpu),
        "metal" => Ok(crate::runtime_adapter::AcceleratorKind::Metal),
        "cuda" => Ok(crate::runtime_adapter::AcceleratorKind::Cuda),
        "rocm" => Ok(crate::runtime_adapter::AcceleratorKind::Rocm),
        "vulkan" => Ok(crate::runtime_adapter::AcceleratorKind::Vulkan),
        "direct_ml" => Ok(crate::runtime_adapter::AcceleratorKind::DirectMl),
        _ => Err(invalid(
            "catalog.requiredAccelerator",
            "is not a supported accelerator identifier",
        )),
    }
}

fn runtime_slug(runtime: M3RuntimeKind) -> &'static str {
    match runtime {
        M3RuntimeKind::Ollama => "ollama",
        M3RuntimeKind::LlamaCpp => "llama_cpp",
        M3RuntimeKind::Mlx => "mlx",
    }
}

async fn run_bounded<T, F>(
    context: &M3OperationContext,
    operation: &str,
    future: F,
) -> M3HubResult<T>
where
    F: Future<Output = M3HubResult<T>>,
{
    context.preflight(operation)?;
    tokio::select! {
        _ = context.cancellation.cancelled() => Err(M3HubError::Cancelled {
            operation: operation.to_string(),
        }),
        result = tokio::time::timeout(std::time::Duration::from_millis(context.timeout_ms), future) => {
            result.map_err(|_| M3HubError::Timeout {
                operation: operation.to_string(),
                timeout_ms: context.timeout_ms,
            })?
        }
    }
}

async fn read_response_bounded(
    mut response: reqwest::Response,
    max_bytes: usize,
    context: &M3OperationContext,
) -> M3HubResult<Vec<u8>> {
    if let Some(length) = response.content_length() {
        if length > max_bytes as u64 {
            return Err(invalid("response.contentLength", "exceeds the body limit"));
        }
    }
    let mut output = Vec::new();
    while let Some(chunk) = run_bounded(context, "read catalog response", async {
        response
            .chunk()
            .await
            .map_err(|error| M3HubError::Transport(error.to_string()))
    })
    .await?
    {
        if output.len().saturating_add(chunk.len()) > max_bytes {
            return Err(invalid("response.body", "exceeds the body limit"));
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

fn load_hub_state(state_root: &Path, models_root: &Path) -> M3HubResult<M3HubState> {
    let mut candidates = Vec::new();
    for entry in
        fs::read_dir(state_root).map_err(|source| io_at("list M3 hub state", state_root, source))?
    {
        let entry = entry.map_err(|source| io_at("read M3 hub state entry", state_root, source))?;
        let Some((generation, digest_prefix)) = parse_state_filename(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_at("inspect M3 hub state", &path, source))?;
        if !metadata.file_type().is_file() {
            return Err(M3HubError::State(
                "state generation is not a regular file".to_string(),
            ));
        }
        candidates.push((generation, digest_prefix, path, metadata.len()));
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let Some((filename_generation, digest_prefix, path, size)) = candidates.first() else {
        return Ok(M3HubState::default());
    };
    if *size > MAX_STATE_BYTES as u64 {
        return Err(M3HubError::State(
            "state generation exceeds the byte limit".to_string(),
        ));
    }
    let bytes = fs::read(path).map_err(|source| io_at("read M3 hub state", path, source))?;
    let actual_digest = sha256_hex(&bytes);
    if !actual_digest.starts_with(digest_prefix) {
        return Err(M3HubError::State(
            "state filename digest does not match its bytes".to_string(),
        ));
    }
    let state: M3HubState = serde_json::from_slice(&bytes)?;
    if state.generation != *filename_generation {
        return Err(M3HubError::State(
            "state filename generation does not match its payload".to_string(),
        ));
    }
    validate_hub_state(&state, models_root)?;
    Ok(state)
}

fn save_next_hub_state(state_root: &Path, state: &mut M3HubState, now_ms: u64) -> M3HubResult<()> {
    validate_timestamp(now_ms, "state.updatedAtMs")?;
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| M3HubError::State("state generation overflow".to_string()))?;
    state.updated_at_ms = now_ms;
    validate_hub_state_structure(state)?;
    let bytes = canonical_json(state)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(M3HubError::State(
            "state exceeds the byte limit".to_string(),
        ));
    }
    let digest = sha256_hex(&bytes);
    let path = state_root.join(format!(
        "{STATE_PREFIX}{:020}-{}{STATE_SUFFIX}",
        state.generation,
        &digest[..16]
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|source| io_at("create M3 hub state", &path, source))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| io_at("write M3 hub state", &path, source))?;
    sync_directory(state_root)?;
    prune_state_generations(state_root, state.generation)?;
    Ok(())
}

fn validate_hub_state(state: &M3HubState, models_root: &Path) -> M3HubResult<()> {
    validate_hub_state_structure(state)?;
    for stored in &state.models {
        for version in &stored.versions {
            let artifact = models_root.join(&version.artifact_relative_path);
            ensure_descendant(models_root, &artifact)?;
            let metadata = fs::symlink_metadata(&artifact)
                .map_err(|source| io_at("inspect installed model", &artifact, source))?;
            if !metadata.file_type().is_file() || metadata.len() != version.model.size_bytes {
                return Err(M3HubError::State(format!(
                    "installed model {} has missing or invalid payload metadata",
                    stored.asset_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_hub_state_structure(state: &M3HubState) -> M3HubResult<()> {
    if state.state_version != M3_HUB_STATE_VERSION {
        return Err(M3HubError::State(
            "unsupported M3 hub state version".to_string(),
        ));
    }
    if state.generation == 0 {
        if state.updated_at_ms != 0
            || !state.models.is_empty()
            || !state.runtime_configs.is_empty()
            || state.lan_policy.is_some()
        {
            return Err(M3HubError::State(
                "generation zero is reserved for empty in-memory state".to_string(),
            ));
        }
    } else {
        validate_timestamp(state.updated_at_ms, "state.updatedAtMs")?;
    }
    if state.models.len() > MAX_INSTALLED_MODELS {
        return Err(M3HubError::State(
            "installed model count exceeds the limit".to_string(),
        ));
    }
    let mut asset_ids = BTreeSet::new();
    let mut asset_keys = BTreeSet::new();
    for stored in &state.models {
        validate_identifier(&stored.asset_id, "state.models.assetId")?;
        validate_sha256(&stored.asset_key, "state.models.assetKey")?;
        if stored.asset_key != sha256_hex(stored.asset_id.as_bytes()) {
            return Err(M3HubError::State(
                "stored asset key does not derive from its id".to_string(),
            ));
        }
        if !asset_ids.insert(&stored.asset_id) || !asset_keys.insert(&stored.asset_key) {
            return Err(M3HubError::State(
                "installed model ids and keys must be unique".to_string(),
            ));
        }
        if stored.versions.is_empty() || stored.versions.len() > MAX_MODEL_VERSIONS {
            return Err(M3HubError::State(
                "installed model version count is invalid".to_string(),
            ));
        }
        let mut version_keys = BTreeSet::new();
        for version in &stored.versions {
            version.model.validate()?;
            if version.model.asset_id() != stored.asset_id
                || version.version_key != version.model.version_key()
            {
                return Err(M3HubError::State(
                    "installed version identity differs from its catalog record".to_string(),
                ));
            }
            validate_sha256(&version.version_key, "state.models.versionKey")?;
            validate_timestamp(version.installed_at_ms, "state.models.installedAtMs")?;
            if version.artifact_relative_path
                != relative_model_payload(&stored.asset_key, &version.version_key)
            {
                return Err(M3HubError::State(
                    "installed artifact path is not canonical".to_string(),
                ));
            }
            validate_relative_path(&version.artifact_relative_path)?;
            if !version_keys.insert(&version.version_key) {
                return Err(M3HubError::State(
                    "installed version keys must be unique".to_string(),
                ));
            }
        }
        if !version_keys.contains(&stored.active_version_key) {
            return Err(M3HubError::State(
                "active version does not exist".to_string(),
            ));
        }
    }
    if state.runtime_configs.len() > MAX_RUNTIME_COUNT {
        return Err(M3HubError::State(
            "runtime config count exceeds the limit".to_string(),
        ));
    }
    for runtime_id in state.runtime_configs.keys() {
        validate_identifier(runtime_id, "state.runtimeConfigs.runtimeId")?;
    }
    if let Some(policy) = &state.lan_policy {
        policy.validate().map_err(M3HubError::from)?;
    }
    Ok(())
}

fn parse_state_filename(name: &std::ffi::OsStr) -> Option<(u64, String)> {
    let name = name.to_str()?;
    let middle = name
        .strip_prefix(STATE_PREFIX)?
        .strip_suffix(STATE_SUFFIX)?;
    let (generation, digest) = middle.split_once('-')?;
    if generation.len() != 20
        || digest.len() != 16
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    Some((generation.parse().ok()?, digest.to_string()))
}

fn prune_state_generations(state_root: &Path, current_generation: u64) -> M3HubResult<()> {
    let mut generations = Vec::new();
    for entry in fs::read_dir(state_root)
        .map_err(|source| io_at("list M3 state generations", state_root, source))?
    {
        let entry =
            entry.map_err(|source| io_at("read M3 state generation", state_root, source))?;
        let Some((generation, _)) = parse_state_filename(&entry.file_name()) else {
            continue;
        };
        if generation <= current_generation {
            generations.push((generation, entry.path()));
        }
    }
    generations.sort_by_key(|generation| std::cmp::Reverse(generation.0));
    for (_, path) in generations.into_iter().skip(KEPT_STATE_GENERATIONS) {
        remove_owned_file(&path)?;
    }
    sync_directory(state_root)
}

fn prepare_resume_files(
    partial_path: &Path,
    resume_path: &Path,
    expected: &M3ResumeState,
    probe: &M3DownloadProbe,
) -> M3HubResult<()> {
    let partial_exists = inspect_optional_regular(partial_path)?;
    let resume_exists = inspect_optional_regular(resume_path)?;
    let mut reset = partial_exists != resume_exists;
    if resume_exists {
        let bytes = read_regular_bounded(resume_path, 256 * 1024)?;
        let existing: M3ResumeState = serde_json::from_slice(&bytes)?;
        reset |= existing != *expected;
    }
    if partial_exists {
        let size = regular_file_len_or_zero(partial_path)?;
        reset |= size > expected.total_bytes;
        reset |= size > 0 && (!probe.accepts_ranges || probe.etag.is_none());
    }
    if reset {
        remove_owned_file(partial_path)?;
        remove_owned_file(resume_path)?;
    }
    Ok(())
}

fn validate_download_chunk(
    chunk: &M3DownloadChunk,
    probe: &M3DownloadProbe,
    expected_offset: u64,
    requested: usize,
) -> M3HubResult<()> {
    if chunk.offset != expected_offset {
        return Err(invalid(
            "download.chunk.offset",
            "server returned a non-contiguous range",
        ));
    }
    if chunk.total_bytes != probe.total_bytes {
        return Err(invalid(
            "download.chunk.totalBytes",
            "resource length changed during download",
        ));
    }
    if chunk.bytes.is_empty() || chunk.bytes.len() > requested {
        return Err(invalid(
            "download.chunk.bytes",
            "chunk is empty or exceeds the requested range",
        ));
    }
    if expected_offset
        .checked_add(chunk.bytes.len() as u64)
        .is_none_or(|end| end > probe.total_bytes)
    {
        return Err(invalid(
            "download.chunk.bytes",
            "chunk exceeds the declared resource length",
        ));
    }
    if let Some(expected_etag) = probe.etag.as_deref() {
        if chunk.etag.as_deref() != Some(expected_etag) {
            return Err(invalid(
                "download.chunk.etag",
                "resource identity changed during download",
            ));
        }
    }
    Ok(())
}

fn verify_stored_model(stored: &M3StoredModel, models_root: &Path) -> M3HubResult<()> {
    for version in &stored.versions {
        let directory = models_root
            .join(&stored.asset_key)
            .join(&version.version_key);
        verify_model_directory(&directory, &version.model)?;
    }
    Ok(())
}

fn verify_model_directory(directory: &Path, expected: &M3CatalogModel) -> M3HubResult<()> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|source| io_at("inspect model version", directory, source))?;
    if !metadata.file_type().is_dir() {
        return Err(M3HubError::State(
            "model version is not a real directory".to_string(),
        ));
    }
    let mut names = BTreeSet::new();
    for entry in
        fs::read_dir(directory).map_err(|source| io_at("list model version", directory, source))?
    {
        let entry = entry.map_err(|source| io_at("read model version entry", directory, source))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| M3HubError::State("model entry name is not UTF-8".to_string()))?
            .to_string();
        let entry_metadata = entry
            .metadata()
            .map_err(|source| io_at("inspect model version entry", &entry.path(), source))?;
        if !entry_metadata.is_file()
            || !matches!(name.as_str(), MODEL_PAYLOAD_FILE | MODEL_MANIFEST_FILE)
        {
            return Err(M3HubError::State(
                "model version contains an unexpected entry".to_string(),
            ));
        }
        names.insert(name);
    }
    if names
        != BTreeSet::from([
            MODEL_MANIFEST_FILE.to_string(),
            MODEL_PAYLOAD_FILE.to_string(),
        ])
    {
        return Err(M3HubError::State(
            "model version is missing required files".to_string(),
        ));
    }
    let manifest_path = directory.join(MODEL_MANIFEST_FILE);
    let manifest: M3CatalogModel =
        serde_json::from_slice(&read_regular_bounded(&manifest_path, MAX_STATE_BYTES)?)?;
    if &manifest != expected {
        return Err(M3HubError::State(
            "model manifest differs from authenticated hub state".to_string(),
        ));
    }
    let payload = directory.join(MODEL_PAYLOAD_FILE);
    let digest = sha256_file(&payload, expected.size_bytes)?;
    if !constant_time_eq(digest.as_bytes(), expected.sha256.as_bytes()) {
        return Err(M3HubError::Integrity {
            expected: expected.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

fn verify_asset_root(root: &Path, stored: &M3StoredModel) -> M3HubResult<()> {
    verify_stored_model(
        stored,
        root.parent()
            .ok_or_else(|| invalid("assetRoot", "has no parent"))?,
    )?;
    let expected = stored
        .versions
        .iter()
        .map(|version| version.version_key.as_str())
        .collect::<BTreeSet<_>>();
    let mut observed = BTreeSet::new();
    for entry in fs::read_dir(root).map_err(|source| io_at("list asset root", root, source))? {
        let entry = entry.map_err(|source| io_at("read asset root entry", root, source))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|source| io_at("inspect asset root entry", &entry.path(), source))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| M3HubError::State("asset entry name is not UTF-8".to_string()))?
            .to_string();
        if !metadata.file_type().is_dir() || !expected.contains(name.as_str()) {
            return Err(M3HubError::State(
                "asset root contains an unexpected entry".to_string(),
            ));
        }
        observed.insert(name);
    }
    if observed.len() != expected.len() {
        return Err(M3HubError::State(
            "asset root is missing a version directory".to_string(),
        ));
    }
    Ok(())
}

fn relative_model_payload(asset_key: &str, version_key: &str) -> String {
    format!("{asset_key}/{version_key}/{MODEL_PAYLOAD_FILE}")
}

fn sha256_file(path: &Path, expected_size: u64) -> M3HubResult<String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_at("inspect checksum input", path, source))?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(M3HubError::State(
            "checksum input is not a regular file of the expected size".to_string(),
        ));
    }
    let mut file = File::open(path).map_err(|source| io_at("open checksum input", path, source))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut observed = 0_u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| io_at("read checksum input", path, source))?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read as u64)
            .ok_or_else(|| M3HubError::State("checksum byte count overflow".to_string()))?;
        if observed > expected_size {
            return Err(M3HubError::State(
                "checksum input grew during verification".to_string(),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    if observed != expected_size {
        return Err(M3HubError::State(
            "checksum input changed size during verification".to_string(),
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn directory_size(root: &Path) -> M3HubResult<u64> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|source| io_at("inspect managed storage", root, source))?;
    if !metadata.file_type().is_dir() {
        return Err(M3HubError::State(
            "managed storage root is not a real directory".to_string(),
        ));
    }
    let mut total = 0_u64;
    for entry in fs::read_dir(root).map_err(|source| io_at("list managed storage", root, source))? {
        let entry = entry.map_err(|source| io_at("read managed storage entry", root, source))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_at("inspect managed storage entry", &path, source))?;
        let bytes = if metadata.file_type().is_dir() {
            directory_size(&path)?
        } else if metadata.file_type().is_file() {
            metadata.len()
        } else {
            return Err(M3HubError::State(
                "managed storage contains a symlink or special file".to_string(),
            ));
        };
        total = total
            .checked_add(bytes)
            .ok_or_else(|| M3HubError::State("managed storage byte count overflow".to_string()))?;
    }
    Ok(total)
}

fn regular_file_len_or_zero(path: &Path) -> M3HubResult<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Err(M3HubError::State(
            "managed file path is not a regular file".to_string(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(source) => Err(io_at("inspect managed file", path, source)),
    }
}

fn inspect_optional_regular(path: &Path) -> M3HubResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(M3HubError::State(
            "managed path is not a regular file".to_string(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(io_at("inspect managed path", path, source)),
    }
}

fn read_regular_bounded(path: &Path, max_bytes: usize) -> M3HubResult<Vec<u8>> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| io_at("inspect bounded file", path, source))?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes as u64 {
        return Err(M3HubError::State(
            "bounded file is not regular or exceeds its limit".to_string(),
        ));
    }
    let bytes = fs::read(path).map_err(|source| io_at("read bounded file", path, source))?;
    if bytes.len() > max_bytes {
        return Err(M3HubError::State(
            "bounded file grew beyond its limit".to_string(),
        ));
    }
    Ok(bytes)
}

fn remove_owned_file(path: &Path) -> M3HubResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            fs::remove_file(path).map_err(|source| io_at("remove owned file", path, source))
        }
        Ok(_) => Err(M3HubError::State(
            "refusing to remove a non-regular managed path".to_string(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_at("inspect owned file", path, source)),
    }
}

fn remove_owned_directory(path: &Path) -> M3HubResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_at("inspect owned directory", path, source))?;
    if !metadata.file_type().is_dir() {
        return Err(M3HubError::State(
            "refusing to remove a non-directory managed path".to_string(),
        ));
    }
    verify_plain_tree(path)?;
    fs::remove_dir_all(path).map_err(|source| io_at("remove owned directory", path, source))
}

fn restore_isolated_versions(isolated: &[(PathBuf, PathBuf)], asset_root: &Path) {
    for (isolated_path, original_path) in isolated.iter().rev() {
        let _ = fs::rename(isolated_path, original_path);
    }
    let _ = sync_directory(asset_root);
}

fn verify_plain_tree(path: &Path) -> M3HubResult<()> {
    for entry in fs::read_dir(path).map_err(|source| io_at("list owned tree", path, source))? {
        let entry = entry.map_err(|source| io_at("read owned tree entry", path, source))?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)
            .map_err(|source| io_at("inspect owned tree entry", &entry_path, source))?;
        if metadata.file_type().is_dir() {
            verify_plain_tree(&entry_path)?;
        } else if !metadata.file_type().is_file() {
            return Err(M3HubError::State(
                "owned tree contains a symlink or special file".to_string(),
            ));
        }
    }
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> M3HubResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("path", "has no parent"))?;
    ensure_private_directory(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.file_type().is_file() {
            return Err(M3HubError::State(
                "atomic write target is not a regular file".to_string(),
            ));
        }
    }
    let temporary = parent.join(format!(".m3-write-{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .map_err(|source| io_at("create atomic M3 file", &temporary, source))?;
    if let Err(source) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        let _ = fs::remove_file(&temporary);
        return Err(io_at("write atomic M3 file", &temporary, source));
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_at("publish atomic M3 file", path, source));
    }
    sync_directory(parent)
}

fn ensure_private_directory(path: &Path) -> M3HubResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(M3HubError::State(
                "private path is not a real directory".to_string(),
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| io_at("create private M3 directory", path, source))?,
        Err(source) => return Err(io_at("inspect private M3 directory", path, source)),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_at("secure private M3 directory", path, source))?;
    Ok(())
}

fn harden_file(path: &Path) -> M3HubResult<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| io_at("inspect private M3 file", path, source))?;
    if !metadata.file_type().is_file() {
        return Err(M3HubError::State(
            "private M3 file is not regular".to_string(),
        ));
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_at("secure private M3 file", path, source))?;
    Ok(())
}

fn sync_directory(path: &Path) -> M3HubResult<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_at("sync M3 directory", path, source))?;
    Ok(())
}

fn ensure_descendant(root: &Path, path: &Path) -> M3HubResult<()> {
    if !path.starts_with(root) || path == root {
        return Err(M3HubError::State(
            "managed path escaped its storage root".to_string(),
        ));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> M3HubResult<()> {
    if value.is_empty()
        || value.len() > 4_096
        || value.contains('\0')
        || value.contains('\\')
        || value.starts_with('/')
        || value.ends_with('/')
        || value
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(invalid("relativePath", "is not normalized and safe"));
    }
    if Path::new(value)
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid("relativePath", "contains traversal"));
    }
    Ok(())
}

fn validate_download_url(value: &str) -> M3HubResult<()> {
    validate_https_url(value, "downloadUrl", false)
}

fn validate_https_url(value: &str, field: &str, allow_loopback_http: bool) -> M3HubResult<()> {
    if value.len() > 16 * 1024 || value.contains('\0') {
        return Err(invalid(field, "is empty, oversized, or contains NUL"));
    }
    let parsed = Url::parse(value).map_err(|error| invalid(field, error.to_string()))?;
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.host().is_none()
    {
        return Err(invalid(
            field,
            "must not contain credentials/fragments and must have a host",
        ));
    }
    let loopback = match parsed.host() {
        Some(Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    let valid_scheme = parsed.scheme() == "https"
        || (allow_loopback_http && parsed.scheme() == "http" && loopback);
    if !valid_scheme {
        return Err(invalid(
            field,
            "must use HTTPS (loopback HTTP is allowed only for configured catalogs)",
        ));
    }
    Ok(())
}

fn parse_content_range(value: &HeaderValue) -> M3HubResult<(u64, u64, u64)> {
    let value = value
        .to_str()
        .map_err(|_| invalid("download.contentRange", "is not valid ASCII"))?;
    let rest = value
        .strip_prefix("bytes ")
        .ok_or_else(|| invalid("download.contentRange", "must use byte units"))?;
    let (range, total) = rest
        .split_once('/')
        .ok_or_else(|| invalid("download.contentRange", "is malformed"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| invalid("download.contentRange", "is malformed"))?;
    let start = start
        .parse::<u64>()
        .map_err(|error| invalid("download.contentRange", error.to_string()))?;
    let end = end
        .parse::<u64>()
        .map_err(|error| invalid("download.contentRange", error.to_string()))?;
    let total = total
        .parse::<u64>()
        .map_err(|error| invalid("download.contentRange", error.to_string()))?;
    if end < start || end >= total || total == 0 || total > MAX_DOWNLOAD_BYTES {
        return Err(invalid(
            "download.contentRange",
            "contains impossible bounds",
        ));
    }
    Ok((start, end, total))
}

fn header_u64(value: Option<&HeaderValue>, field: &str) -> M3HubResult<u64> {
    value
        .ok_or_else(|| invalid(field, "is missing"))?
        .to_str()
        .map_err(|_| invalid(field, "is not valid ASCII"))?
        .parse::<u64>()
        .map_err(|error| invalid(field, error.to_string()))
}

fn optional_header(value: Option<&HeaderValue>, field: &str) -> M3HubResult<Option<String>> {
    value
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| invalid(field, "is not valid ASCII"))?;
            if value.is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
                return Err(invalid(field, "is empty, oversized, or contains controls"));
            }
            Ok(value.to_string())
        })
        .transpose()
}

fn validate_search(query: &str, limit: usize) -> M3HubResult<()> {
    if query.trim().is_empty() || query.len() > 1_024 || query.contains('\0') {
        return Err(invalid("query", "must contain 1..=1024 bytes without NUL"));
    }
    if limit == 0 || limit > MAX_CATALOG_ENTRIES {
        return Err(invalid(
            "limit",
            format!("must be between 1 and {MAX_CATALOG_ENTRIES}"),
        ));
    }
    Ok(())
}

fn validate_identifier(value: &str, field: &str) -> M3HubResult<()> {
    if value.is_empty()
        || value.len() > MAX_IDENTIFIER_BYTES
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        Err(invalid(
            field,
            format!("must contain 1..={MAX_IDENTIFIER_BYTES} bytes without controls"),
        ))
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, field: &str, max_bytes: usize) -> M3HubResult<()> {
    if value.len() > max_bytes || value.contains('\0') {
        Err(invalid(
            field,
            format!("exceeds {max_bytes} bytes or contains NUL"),
        ))
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str, field: &str) -> M3HubResult<()> {
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

fn validate_timestamp(value: u64, field: &str) -> M3HubResult<()> {
    if value == 0 || value > i64::MAX as u64 {
        Err(invalid(field, "must be a positive signed-64-bit timestamp"))
    } else {
        Ok(())
    }
}

fn canonical_json<T: Serialize + ?Sized>(value: &T) -> M3HubResult<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    canonicalize_json(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn canonical_json_string<T: Serialize + ?Sized>(value: &T) -> M3HubResult<String> {
    String::from_utf8(canonical_json(value)?)
        .map_err(|error| invalid("json", format!("canonical JSON is not UTF-8: {error}")))
}

fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize_json),
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut entries = previous.into_iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in entries {
                canonicalize_json(&mut value);
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn lock<T>(mutex: &Mutex<T>) -> M3HubResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| M3HubError::LockPoisoned)
}

fn read_lock<T>(lock: &RwLock<T>) -> M3HubResult<RwLockReadGuard<'_, T>> {
    lock.read().map_err(|_| M3HubError::LockPoisoned)
}

fn write_lock<T>(lock: &RwLock<T>) -> M3HubResult<RwLockWriteGuard<'_, T>> {
    lock.write().map_err(|_| M3HubError::LockPoisoned)
}

fn invalid(field: impl Into<String>, message: impl Into<String>) -> M3HubError {
    M3HubError::Invalid {
        field: field.into(),
        message: message.into(),
    }
}

fn runtime_error(error: crate::runtime_adapter::RuntimeAdapterError) -> M3HubError {
    M3HubError::Runtime(error.to_string())
}

fn mlx_error(error: crate::mlx_runtime::MlxError) -> M3HubError {
    M3HubError::Runtime(error.to_string())
}

fn stream_sink_error(message: String) -> M3HubError {
    M3HubError::Runtime(format!("stream sink: {message}"))
}

fn io_at(operation: &'static str, path: &Path, source: io::Error) -> M3HubError {
    M3HubError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}
