//! M3 model/runtime/API integration service.
//!
//! This module deliberately contains no Tauri global state or HTTP listener.
//! It is the shared service boundary used by desktop commands, the local API,
//! and tests. Network, hardware, runtime-process, inference, secret-protection,
//! and clock effects are injected so callers can provide platform integrations
//! without weakening the validation and persistence rules here.

use crate::compatibility_hub::{
    compatibility_conformance_manifest, encode_embeddings_response, encode_ollama_chat_response,
    encode_response, encode_stream_event, translate_embeddings_request,
    translate_ollama_chat_request, ApiBackend, ApiScope, AuthorizationRequest,
    AuthorizedBackendCandidates, AuthorizedStagedRequest, AuthorizedToken,
    BackendCandidateAuthorizationRequest, CanonicalEmbeddingRequest, CanonicalEmbeddingResponse,
    CanonicalInferenceRequest, CanonicalInferenceResponse, CanonicalStreamEvent,
    CompatibilityConformanceManifest, CompatibilityError, CompatibilityProtocol,
    CredentialPreflightRequest, LanAccessController, LanEntropySource, LanServerPolicy,
    LanStateProtector, PairedToken, PairingChallengeView, PairingRequest, ProtocolStreamFrame,
    ScopedTokenView, SecurityAuditEvent, StagedAuthorizationRequest,
};
// MLX is Metal-only, so `crate::mlx_runtime` exists only in the macOS build and
// every hub item that names one of its types is gated to match.
#[cfg(target_os = "macos")]
use crate::mlx_runtime::{
    MlxGenerationRequest, MlxGenerationSummary, MlxMessage, MlxOperationContext, MlxProcessMetrics,
    MlxRuntimeAdapter, MlxRuntimeStatus, MlxStreamEvent, MlxStreamSink, MlxToolDefinition,
};
// Reached only by the MLX driver and `canonical_message_to_mlx` outside of
// tests, which reuse them for the non-MLX collector fixtures.
#[cfg(any(target_os = "macos", test))]
use crate::compatibility_hub::{
    request_offers_tool, CanonicalContent, CanonicalMessage, CanonicalRole, CanonicalUsage,
};
use crate::runtime_adapter::{
    validate_setting_values, AcceleratorKind, AdvancedSettingCapability, HardwareProfile,
    HardwareSnapshot, KeepAlive, ModelLoadRequest, ModelUnloadRequest, RunningModel,
    RuntimeAdapter, RuntimeCapabilities, RuntimeInventory, RuntimeLogRequest, RuntimeLogTail,
    RuntimeOperationContext, RuntimeOperationLimits, RuntimeStatus, SettingValue, UnloadPolicy,
};
use reqwest::header::{
    HeaderValue, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, RANGE,
};
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "macos", test))]
use serde_json::json;
use serde_json::Value;
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
    /// A request refused for exceeding the running process's context budget,
    /// carrying the process class's stated policy (roadmap K11).
    ///
    /// Its own variant rather than a `Runtime(String)`, because the two answers
    /// differ where it matters: `Runtime` means this app's runtime failed and
    /// maps to a 502, while an over-budget prompt is a request the client sent
    /// that this app declined to forward — nothing failed, and the client is the
    /// only party that can shorten it. `code` is `ContextPolicy::code()`, or a
    /// bare `context_budget` when the process has no run to derive a class from.
    ContextBudget {
        code: &'static str,
        message: String,
    },
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
            // No prefix: this one is written for the person reading it and is
            // already a complete sentence naming both numbers and what to do.
            Self::ContextBudget { message, .. } => write!(formatter, "{message}"),
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

/// A minimal reference to a multimodal projector asset associated with a
/// model (e.g. a CLIP/SigLIP vision tower or an audio encoder). This only
/// tracks provenance/identity for the manifest; the projector/vision model
/// manager itself (download, placement, sizing) is separate Phase 8 scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ProjectorRef {
    pub kind: String,
    pub sha256: String,
    pub size_bytes: u64,
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
    /// Chat template family/name identifying how this model expects prompts
    /// to be rendered (e.g. "chatml", "llama-3"). Absent for older manifests
    /// and for models whose runtime handles templating internally.
    #[serde(default)]
    pub template: Option<String>,
    /// The multimodal projector associated with this model, if any.
    #[serde(default)]
    pub projector: Option<M3ProjectorRef>,
    /// When this catalog entry was locally retrieved from its source,
    /// stamped by the hub at search time (never trusted from the remote
    /// payload). Absent for manifests installed before this field existed.
    #[serde(default)]
    pub catalog_retrieved_at_ms: Option<u64>,
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
        validate_download_url(&self.download_url, false)?;
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
        if let Some(template) = &self.template {
            validate_identifier(template, "catalog.template")?;
        }
        if let Some(projector) = &self.projector {
            validate_identifier(&projector.kind, "catalog.projector.kind")?;
            validate_sha256(&projector.sha256, "catalog.projector.sha256")?;
            if projector.size_bytes == 0 || projector.size_bytes > MAX_DOWNLOAD_BYTES {
                return Err(invalid(
                    "catalog.projector.sizeBytes",
                    format!("must be between 1 and {MAX_DOWNLOAD_BYTES}"),
                ));
            }
        }
        if let Some(retrieved_at_ms) = self.catalog_retrieved_at_ms {
            validate_timestamp(retrieved_at_ms, "catalog.catalogRetrievedAtMs")?;
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

/// Hardware Compatibility Matrix / "Driver Doctor" status for a single
/// accelerator backend. Every backend is queried defensively: a missing tool,
/// an absent device, or an unsupported OS/arch combination is a normal,
/// expected outcome and must never surface as an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3AcceleratorStatus {
    /// The backend was queried directly and at least one usable device was
    /// found.
    Available,
    /// The backend's tooling ran successfully but reported no device.
    NotDetected,
    /// A device was found, but its driver/compute capability is below the
    /// minimum this app requires for that backend.
    DriverTooOld,
    /// The backend's detection tool (e.g. `nvidia-smi`, `rocm-smi`,
    /// `vulkaninfo`) is not installed or not on `PATH`, so the backend could
    /// not be queried at all.
    ToolMissing,
    /// The backend cannot run on this OS/architecture combination.
    Unsupported,
}

/// One row of the hardware compatibility report: a single accelerator
/// backend, what was found, and a human-readable explanation a user can act
/// on (install a driver, update a runtime, or expect a CPU fallback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3AcceleratorCompatibility {
    pub kind: AcceleratorKind,
    pub status: M3AcceleratorStatus,
    /// Short, human-readable explanation of the status (what works, what
    /// falls back to CPU, or what needs a driver/runtime update).
    pub summary: String,
    pub device_names: Vec<String>,
    pub driver_version: Option<String>,
    pub compute_capability: Option<String>,
    /// False when this status was inferred or assumed rather than obtained
    /// from a direct hardware/driver query, so `status: Available` should be
    /// read as "likely" rather than confirmed. Used for Windows DirectML
    /// (only a display adapter's presence is confirmed, not the DirectML
    /// runtime path itself) and for the Metal OS/arch fallback used when
    /// `system_profiler` is unavailable.
    pub confirmed: bool,
    /// Whether anything in this app actually runs work on this backend, or it
    /// is reported for diagnosis only (roadmap K16).
    ///
    /// Distinct from `status` and from `confirmed`, and the distinction is the
    /// whole point. `status` is about the *machine* — is the hardware there,
    /// is the driver new enough. `confirmed` is about the *detection* — was
    /// that answered by a direct query or inferred. This is about **this
    /// build**: a backend can be present, confirmed, and still have nothing
    /// here that executes on it, which was true of three of the five and said
    /// nowhere.
    pub execution: crate::runtime_adapter::ExecutionSupport,
}

/// NVIDIA Jetson (Tegra) detection result. Jetson devices share CUDA
/// tooling with desktop NVIDIA GPUs but have their own driver/runtime
/// implications, so they are reported separately from the CUDA row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3JetsonInfo {
    pub detected: bool,
    pub model: Option<String>,
}

/// Full hardware compatibility report shown to the user before a model
/// download, model load, or runtime install: for every accelerator backend,
/// what will work, what falls back to CPU, and what needs a driver/runtime
/// update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3HardwareCompatibilityReport {
    pub captured_at_ms: u64,
    pub os: String,
    pub arch: String,
    pub accelerators: Vec<M3AcceleratorCompatibility>,
    pub jetson: M3JetsonInfo,
    /// True when more than one GPU-capable backend/device was detected
    /// (e.g. an integrated and a discrete adapter, or two backends both
    /// reporting `Available`) so the UI never silently picks one.
    pub hybrid_graphics_detected: bool,
    /// Free-form notes: unsupported runtime combinations, mixed-vendor GPU
    /// warnings, and other callouts that do not fit a single backend row.
    pub notes: Vec<String>,
}

/// Derives a best-effort compatibility report purely from a [`HardwareSnapshot`]'s
/// accelerator list. This is the default used by any [`M3HardwareProbe`] that
/// does not override [`M3HardwareProbe::compatibility_report`]; production
/// probes override it to add driver versions, compute capability, Jetson
/// detection, and the `ToolMissing`/`DriverTooOld` distinctions a plain
/// accelerator list cannot express.
pub fn compatibility_report_from_snapshot(
    snapshot: &HardwareSnapshot,
) -> M3HardwareCompatibilityReport {
    // Every backend this app has an opinion about, including the one it never
    // probes: `AppleNeuralEngine` is on the list so the report *states* that
    // nothing here executes on it (roadmap K16). It can never be `Available`
    // — `PlatformCapabilities::from_host` refuses it into the snapshot — so the
    // loop below resolves it to `NotDetected` with `execution` carrying the
    // reason, which is the honest pair.
    //
    // Kept in step with `m3_production`'s richer builder by
    // `system_hardware_probe_compatibility_report_matches_hub_accessor`, which
    // is what caught this list when only the other one had grown.
    let backends = [
        AcceleratorKind::Metal,
        AcceleratorKind::Cuda,
        AcceleratorKind::Rocm,
        AcceleratorKind::Vulkan,
        AcceleratorKind::DirectMl,
        AcceleratorKind::AppleNeuralEngine,
    ];
    let mut accelerators = Vec::with_capacity(backends.len());
    for kind in backends {
        let found = snapshot
            .platform
            .accelerators
            .iter()
            .find(|capability| capability.kind == kind);
        let (status, summary, device_names) = match found {
            Some(capability) if capability.available => (
                M3AcceleratorStatus::Available,
                format!("{kind:?} reported available by the hardware snapshot."),
                capability.device_names.clone(),
            ),
            _ => (
                M3AcceleratorStatus::NotDetected,
                format!("{kind:?} was not detected on this platform; falls back to CPU."),
                Vec::new(),
            ),
        };
        accelerators.push(M3AcceleratorCompatibility {
            kind,
            execution: crate::runtime_adapter::execution_support(kind),
            status,
            summary,
            device_names,
            driver_version: None,
            compute_capability: None,
            confirmed: true,
        });
    }
    let available_backends = accelerators
        .iter()
        .filter(|entry| entry.status == M3AcceleratorStatus::Available)
        .count();
    M3HardwareCompatibilityReport {
        captured_at_ms: snapshot.captured_at_ms,
        os: snapshot.platform.os.clone(),
        arch: snapshot.platform.arch.clone(),
        accelerators,
        jetson: M3JetsonInfo {
            detected: false,
            model: None,
        },
        hybrid_graphics_detected: available_backends > 1,
        notes: Vec::new(),
    }
}

pub trait M3HardwareProbe: Send + Sync {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot>;

    /// Hardware Compatibility Matrix / "Driver Doctor" report shown to the
    /// user before a model download, model load, or runtime install. The
    /// default implementation derives a coarse report from [`Self::snapshot`];
    /// production probes should override this to add real driver-version and
    /// compute-capability detection. Implementations must never panic or
    /// return an error merely because a GPU tool/driver is absent — that is
    /// the normal `NotDetected`/`ToolMissing` case.
    fn compatibility_report(&self) -> M3HubResult<M3HardwareCompatibilityReport> {
        Ok(compatibility_report_from_snapshot(&self.snapshot()?))
    }
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
                crate::egress::send(self.client.get(url))
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

/// Names this path in a denial record, so a hop refused here stays
/// distinguishable from the same address class refused by `web.rs` or
/// `model_sources.rs`.
pub(crate) const COMPONENT_EGRESS_GUARD: &str = "m3.component-download";

/// Turns a response reqwest could not follow into an error that says so.
///
/// A `3xx` reaches a caller only when reqwest declined to follow it — no
/// `Location`, or one that will not parse — because a hop that *was* followed is
/// answered by the destination and a hop that was *refused* arrives as a transport
/// error carrying the [`crate::egress::EgressRule`] that refused it. Both of the
/// remaining shapes are malformed, and neither may be reported as the plain HTTP
/// failure it is not: the whole point of this path is that a blocked or broken
/// redirect can never be mistaken for a successful fetch of the pre-redirect page.
fn refuse_unfollowable_redirect(response: &reqwest::Response, field: &str) -> M3HubResult<()> {
    if response.status().is_redirection() {
        return Err(invalid(
            field,
            format!(
                "answered HTTP {} without a Location this app could follow",
                response.status()
            ),
        ));
    }
    Ok(())
}

pub struct ReqwestM3DownloadTransport {
    client: reqwest::Client,
    allow_loopback: bool,
}

impl ReqwestM3DownloadTransport {
    pub fn new() -> M3HubResult<Self> {
        // A published release asset has one stable URL and answers it with a
        // cross-origin `302` to a signed, expiring CDN URL. The client this used
        // to build had `Policy::none()`, so that hop was simply an error — which
        // is why the MLX runtime this project publishes could be listed and never
        // installed. `egress::public_download_client` follows it and re-checks
        // every hop, and every resolved address, against the public-destination
        // rule the initial URL had to pass. See its doc for why that is safe on a
        // digest-verified, credential-free path and would not be as a client
        // default.
        let client = crate::egress::public_download_client(
            crate::egress::PublicDestinations::Only,
            COMPONENT_EGRESS_GUARD,
        )
        .build()
        .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            allow_loopback: false,
        })
    }

    /// [`Self::new`] against a fixture on this machine.
    ///
    /// The *only* difference is which destination class is allowed; the redirect
    /// policy, the hop cap, the resolver and its address pruning are the same
    /// objects production uses, so the end-to-end test below exercises the real
    /// path rather than a parallel one. Kept `cfg(test)` so production has exactly
    /// one answer for where a component may come from, and the reason a fixture
    /// needs this at all is that a fixture listens on loopback — which is what
    /// [`crate::egress::PublicDestinations::Only`] exists to refuse. The
    /// public-side claims are made where they can be made honestly, against the
    /// primitive itself in `egress.rs`.
    #[cfg(test)]
    fn for_loopback_fixture() -> M3HubResult<Self> {
        let client = crate::egress::public_download_client(
            crate::egress::PublicDestinations::LoopbackAllowed,
            COMPONENT_EGRESS_GUARD,
        )
        .build()
        .map_err(|error| M3HubError::Transport(error.to_string()))?;
        Ok(Self {
            client,
            allow_loopback: true,
        })
    }
}

impl M3DownloadTransport for ReqwestM3DownloadTransport {
    fn probe<'a>(
        &'a self,
        url: &'a str,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, M3DownloadProbe> {
        Box::pin(async move {
            validate_download_url(url, self.allow_loopback)?;
            context.preflight("probe model download")?;
            let response = run_bounded(context, "probe model download", async {
                crate::egress::send(self.client.head(url))
                    .await
                    .map_err(transport_error)
            })
            .await?;
            refuse_unfollowable_redirect(&response, "downloadUrl")?;
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
            validate_download_url(url, self.allow_loopback)?;
            context.preflight("download model range")?;
            if max_bytes == 0 || max_bytes > MAX_DOWNLOAD_CHUNK_BYTES {
                return Err(invalid("download.maxBytes", "is outside the safe range"));
            }
            let end = offset
                .checked_add(max_bytes as u64 - 1)
                .ok_or_else(|| invalid("download.range", "overflow"))?;
            // `Range` and `If-Range` are set once and survive every hop, because
            // reqwest re-issues a redirected `GET` with its headers and strips only
            // the four it treats as credentials (`Authorization`, `Cookie`,
            // `Proxy-Authorization`, `WWW-Authenticate`). That matters more here
            // than it reads: a hop that dropped the range would be answered `200`
            // with the whole artifact, and the `PARTIAL_CONTENT` check below would
            // report "byte-range support is required" against a server that
            // supports it perfectly well.
            let request = self
                .client
                .get(url)
                .header(RANGE, format!("bytes={offset}-{end}"));
            let request = match expected_etag {
                Some(etag) => request.header(IF_RANGE, etag),
                None => request,
            };
            let mut response = run_bounded(context, "download model range", async {
                crate::egress::send(request).await.map_err(transport_error)
            })
            .await?;
            refuse_unfollowable_redirect(&response, "downloadUrl")?;
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
    /// The digest of this version's `model.projector` that was last locally
    /// verified against real bytes on disk (see `M3RuntimeHub::verify_projector`).
    /// `None` for a version with no projector, one whose projector has never
    /// been verified, or one installed before this field existed. Compared
    /// against `model.projector.sha256` at view time rather than trusted
    /// blindly, so replacing the catalog's projector reference (a different
    /// digest) without re-verifying correctly reverts to unverified.
    #[serde(default)]
    projector_verified_sha256: Option<String>,
    #[serde(default)]
    projector_verified_at_ms: Option<u64>,
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

/// Where a multimodal projector component genuinely stands relative to a
/// model version that may declare vision/audio capability (ROADMAP Phase 8
/// item 12: Multimodal Projector and Vision Model Manager). Mirrors the
/// honesty bar the Chat Template Compatibility Lab applies to chat/tool/
/// structured-output via `gate_capabilities`: a capability is never shown as
/// ready on faith in a catalog's declared flag alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3ProjectorVerificationState {
    /// This version's declared capabilities do not require a projector.
    NotRequired,
    /// Vision (or another projector-requiring capability) is declared, but
    /// the manifest carries no projector reference at all — the "missing
    /// projector" case this task's acceptance criterion calls out.
    MissingReference,
    /// A projector reference exists, but its declared digest has not been
    /// locally verified against real bytes on disk yet.
    Unverified,
    /// A projector reference exists and its bytes were locally verified
    /// (see `M3RuntimeHub::verify_projector`) against the declared
    /// sha256/size.
    Verified,
}

impl M3ProjectorVerificationState {
    fn resolve(
        requires_projector: bool,
        projector: Option<&M3ProjectorRef>,
        verified_sha256: Option<&str>,
    ) -> Self {
        if !requires_projector {
            return Self::NotRequired;
        }
        match projector {
            None => Self::MissingReference,
            Some(reference) => {
                if verified_sha256 == Some(reference.sha256.as_str()) {
                    Self::Verified
                } else {
                    Self::Unverified
                }
            }
        }
    }
}

/// Whether this hub's own outbound request composition can carry an image
/// content block to the given runtime kind (ROADMAP Phase 8 item 12). This
/// gates `vision_ready` the same way the Chat Template Compatibility Lab's
/// `gate_capabilities` gates chat/tool/structured-output: composing the
/// correct wire shape is the bar this hub can verify from inside its own
/// process boundary — exactly like `fixture_tool_calling` already treats a
/// correctly composed OpenAI-wire/MLX-flattened tool call as "passing"
/// without spinning up the external runtime process itself. Both
/// `openai_messages` (used for the Ollama and managed llama.cpp drivers) and
/// `canonical_message_to_mlx` (the MLX driver) compose real `image_url`/
/// `images` wire content today, so every runtime kind currently supports
/// this; the match stays exhaustive so a future runtime kind must
/// deliberately opt in rather than silently inheriting `true`.
pub fn runtime_supports_image_transport(kind: M3RuntimeKind) -> bool {
    match kind {
        M3RuntimeKind::Ollama | M3RuntimeKind::LlamaCpp | M3RuntimeKind::Mlx => true,
    }
}

/// Estimated resident memory a multimodal projector consumes once loaded,
/// treating its on-disk size as a direct stand-in for resident bytes — the
/// same approximation this hub already applies to whole-model weights
/// (`OffloadModelProfile::weights_bytes` in `runtime_adapter.rs`), just for
/// the much smaller projector component. No invented activation/scratch
/// multiplier is layered on top of it.
pub fn estimated_projector_memory_bytes(projector: &M3ProjectorRef) -> u64 {
    projector.size_bytes
}

/// Where the model hub lives under the Tauri app-data directory.
///
/// Duplicated from `m3_production::M3_DIRECTORY` rather than shared with it,
/// because that module is Tauri-only wiring and this one has to be reachable
/// from the CLI, which links the library without it.
pub const M3_HUB_DIRECTORY: &str = "m3";

/// What a model id will hold once it is resident, or an explicit admission that
/// nothing installed on this machine knows.
///
/// [`Self::Unknown`] exists so a caller cannot mistake "we never measured this
/// model" for "this model costs nothing". Those are the same `u64` and opposite
/// facts: the second may legitimately satisfy a memory bound, the first must
/// never be allowed to look like it did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum M3ModelFootprint {
    Known {
        /// Exact on-disk size of the active version's artifact.
        weights_bytes: u64,
        /// The catalog's declared fully-on-CPU and fully-offloaded footprints.
        memory: crate::runtime_adapter::MemoryRequirement,
        required_accelerator: Option<crate::runtime_adapter::AcceleratorKind>,
        /// Present only when the active version declares a projector.
        projector_memory_bytes: Option<u64>,
    },
    Unknown,
}

/// `model_id` → footprint, read from the hub's own durable installed inventory.
///
/// A free function rather than an [`M3RuntimeHub`] method because the two
/// callers that need it most — the CLI's run submission path and the daemon's
/// admission loop — have an app-data directory and no hub. Building one wants a
/// clock, a hardware probe, a download transport, a catalog list and a runtime
/// inventory, none of which are needed to answer "how big is this model".
///
/// Every failure is [`M3ModelFootprint::Unknown`]: an absent hub root,
/// unreadable state, and a model this machine has never installed are the same
/// answer to the caller, and none of them is worth failing a submission over.
pub fn installed_model_footprint(app_data_dir: &Path, model_id: &str) -> M3ModelFootprint {
    let root = app_data_dir.join(M3_HUB_DIRECTORY);
    let Ok(state) = load_hub_state(&root.join("state"), &root.join("models")) else {
        return M3ModelFootprint::Unknown;
    };
    for stored in &state.models {
        let Some(version) = stored
            .versions
            .iter()
            .find(|version| version.version_key == stored.active_version_key)
        else {
            continue;
        };
        if version.model.model_id != model_id {
            continue;
        }
        return M3ModelFootprint::Known {
            weights_bytes: version.model.size_bytes,
            memory: crate::runtime_adapter::MemoryRequirement {
                ram_bytes: version.model.estimated_ram_bytes,
                vram_bytes: version.model.estimated_vram_bytes,
            },
            required_accelerator: version
                .model
                .required_accelerator
                .as_deref()
                .and_then(|value| parse_accelerator(value).ok()),
            projector_memory_bytes: version
                .model
                .projector
                .as_ref()
                .map(estimated_projector_memory_bytes),
        };
    }
    M3ModelFootprint::Unknown
}

/// The on-disk artifact for `model_id`'s **active** version on this machine, or
/// `None` when this machine has not installed it.
///
/// The companion to [`installed_model_footprint`] directly above: that one
/// answers "how big is it", this one answers "where is it", and both are free
/// functions for the same reason — the callers are a run submission path and a
/// remote route handler that have an app-data directory and no hub.
///
/// `ensure_descendant` is applied for the same reason the hub applies it when
/// building its own views: `artifact_relative_path` comes off durable state, and
/// a path that escaped the models root would hand a caller an arbitrary file to
/// execute a model server against.
#[must_use]
pub fn installed_model_artifact(app_data_dir: &Path, model_id: &str) -> Option<PathBuf> {
    let root = app_data_dir.join(M3_HUB_DIRECTORY);
    let models_root = root.join("models");
    let state = load_hub_state(&root.join("state"), &models_root).ok()?;
    for stored in &state.models {
        // `continue`, not `?`: one model row without an active version must not
        // hide every model listed after it.
        let Some(version) = stored
            .versions
            .iter()
            .find(|version| version.version_key == stored.active_version_key)
        else {
            continue;
        };
        if version.model.model_id != model_id {
            continue;
        }
        let artifact = models_root.join(&version.artifact_relative_path);
        ensure_descendant(&models_root, &artifact).ok()?;
        return artifact.is_file().then_some(artifact);
    }
    None
}

/// Every model this machine has installed, as the placement plane advertises it
/// (roadmap K17 S1).
///
/// The same free-function shape and the same failure direction as
/// [`installed_model_footprint`] directly above, and for the same reason: the
/// caller is a remote route handler with an app-data directory and no hub, and
/// an unreadable inventory is "this node advertises no resident models" rather
/// than a failed request. Sorted by model id so two descriptions of an unchanged
/// node are byte-identical.
#[must_use]
pub fn installed_model_inventory(app_data_dir: &Path) -> Vec<crate::node_placement::NodeModel> {
    let root = app_data_dir.join(M3_HUB_DIRECTORY);
    let Ok(state) = load_hub_state(&root.join("state"), &root.join("models")) else {
        return Vec::new();
    };
    let mut models: Vec<crate::node_placement::NodeModel> = state
        .models
        .iter()
        .filter_map(|stored| {
            let version = stored
                .versions
                .iter()
                .find(|version| version.version_key == stored.active_version_key)?;
            Some(crate::node_placement::NodeModel {
                model_id: version.model.model_id.clone(),
                display_name: version.model.display_name.clone(),
                runtime: format!("{:?}", version.model.runtime).to_ascii_lowercase(),
                weights_bytes: version.model.size_bytes,
                estimated_ram_bytes: version.model.estimated_ram_bytes,
                estimated_vram_bytes: version.model.estimated_vram_bytes,
            })
        })
        .collect();
    models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
    models.dedup_by(|left, right| left.model_id == right.model_id);
    models
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
    /// Which configured catalog source this version originated from.
    pub source_id: String,
    /// Chat template family/name declared by the catalog entry, if any.
    pub template: Option<String>,
    /// The multimodal projector associated with this version, if any.
    pub projector: Option<M3ProjectorRef>,
    /// When the catalog entry for this version was locally retrieved, if the
    /// installing manifest recorded it.
    pub catalog_retrieved_at_ms: Option<u64>,
    /// Real, computed evidence for this version's projector — never trusted
    /// purely from the catalog's declared capability flag.
    pub projector_verification: M3ProjectorVerificationState,
    /// When `projector_verification` last became `Verified`, if it has been.
    pub projector_verified_at_ms: Option<u64>,
    /// Estimated resident memory the projector needs once loaded; present
    /// only when this version actually declares one.
    pub estimated_projector_memory_bytes: Option<u64>,
    /// Whether vision is genuinely ready to use: declared true AND backed by
    /// a verified projector AND the target runtime's own outbound wire
    /// composition can carry an image block. Never true merely because the
    /// catalog set `capabilities.vision = true`.
    pub vision_ready: bool,
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

/// Verifies a candidate local file against an installed model version's
/// declared `M3ProjectorRef` digest/size (ROADMAP Phase 8 item 12). There is
/// deliberately no network download path for the projector blob itself yet
/// — see `M3RuntimeHub::verify_projector`'s doc comment — so this is how a
/// user-supplied or externally-fetched projector file gets promoted from
/// "declared" to genuinely `Verified` evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3VerifyProjectorRequest {
    pub asset_id: String,
    pub version_key: String,
    pub candidate_path: PathBuf,
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
    /// Whether this runtime's transport genuinely reaches an embeddings
    /// endpoint (Ollama's daemon today — the managed llama.cpp chat
    /// instance and MLX do not run with embeddings support). A per-model
    /// `capabilities.embeddings` check still gates individual requests.
    pub can_embed: bool,
    pub settings: Vec<AdvancedSettingCapability>,
}

/// Phase 8 item 11 (OpenAI/Ollama API compatibility harness) result: one
/// row per route × backend × (optionally) model. See
/// [`M3RuntimeHub::compatibility_matrix`] for how rows are derived and
/// `m3_compatibility_harness.rs` for the real HTTP-level regression tests
/// this surfaces the current capability picture for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3CompatibilityStatus {
    Pass,
    Unsupported,
    Fail,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CompatibilityMatrixRow {
    pub method: String,
    pub route: String,
    pub backend: ApiBackend,
    pub runtime_id: String,
    pub model_id: Option<String>,
    pub status: M3CompatibilityStatus,
    pub reason: String,
}

impl M3CompatibilityMatrixRow {
    fn new(
        method: &str,
        route: &str,
        backend: ApiBackend,
        runtime_id: &str,
        model_id: Option<&str>,
        status: M3CompatibilityStatus,
        reason: &str,
    ) -> Self {
        Self {
            method: method.to_string(),
            route: route.to_string(),
            backend,
            runtime_id: runtime_id.to_string(),
            model_id: model_id.map(str::to_string),
            status,
            reason: reason.to_string(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3CompatibilityMatrixReport {
    pub generated_at_ms: u64,
    pub rows: Vec<M3CompatibilityMatrixRow>,
}

fn capability_status(supported: bool) -> M3CompatibilityStatus {
    if supported {
        M3CompatibilityStatus::Pass
    } else {
        M3CompatibilityStatus::Unsupported
    }
}

// -- Sampler, Batching, and Speculative Decoding Controls (ROADMAP Phase 8
// item 17) -------------------------------------------------------------
//
// `AdvancedSettingCapability` (see `runtime_adapter.rs`) already tells the
// UI which knobs a runtime driver knows how to accept at all — that part is
// static per runtime and unaffected by which model or machine it runs on.
// Three of the newer knobs need something the low-level adapter layer
// deliberately has no visibility into:
//   - `flash_attention` / `mixed_precision` (llama.cpp only) can only be
//     honored with a real GPU backend, which is a machine-level fact from
//     the Hardware Compatibility Matrix (`M3HardwareCompatibilityReport`),
//     not something `runtime_adapter.rs`'s Tauri-free adapters probe for
//     themselves (see that report's module doc comment for why hardware
//     probing lives in `m3_production.rs`, not here or there).
//   - `speculative_decoding_draft_model` (llama.cpp only) is inherently
//     relative to *which model* is being configured: it needs a smaller,
//     same-family model already installed to act as the draft. That
//     relationship only exists at this hub layer, which is the only place
//     that knows about every installed model at once.
//
// `gate_advanced_settings` narrows the static capability list down to what
// the current runtime/model/hardware combination can actually honor right
// now, for the UI to render. The same gates are enforced again,
// authoritatively, server-side in `M3RuntimeHub::set_runtime_config` and
// `M3RuntimeHub::load_model` — the UI-facing gating here is a convenience,
// never the only line of defense.

/// Coarse model-family bucket used only to decide whether one installed
/// model could plausibly serve as a speculative-decoding draft model for
/// another — never used for anything else. Keyed by a substring match on
/// `model_id`/`display_name`/`variant_id`, deliberately mirroring
/// `chat_template_lab::TemplateFamily::detect`'s coarse substring approach
/// (see that type's doc comment for why finer-grained detection is out of
/// scope for this codebase). `Generic` never matches another `Generic`
/// model as a compatible pair: two models this app cannot place in a named
/// family are not assumed related just because both are unclassified.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFamily {
    Llama,
    Qwen,
    Mistral,
    Gemma,
    Phi,
    DeepSeek,
    Generic,
}

impl ModelFamily {
    pub fn detect(model_id: &str, display_name: &str, variant_id: &str) -> Self {
        let haystack = format!("{model_id} {display_name} {variant_id}").to_lowercase();
        if haystack.contains("deepseek") {
            Self::DeepSeek
        } else if haystack.contains("qwen") {
            Self::Qwen
        } else if haystack.contains("gemma") {
            Self::Gemma
        } else if haystack.contains("mistral") || haystack.contains("mixtral") {
            Self::Mistral
        } else if haystack.contains("phi") {
            Self::Phi
        } else if haystack.contains("llama") {
            Self::Llama
        } else {
            Self::Generic
        }
    }
}

/// One installed model that could serve as a speculative-decoding draft
/// model for the target the settings are being resolved against.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3DraftModelCandidate {
    pub model_id: String,
    pub display_name: String,
}

/// Result of [`gate_advanced_settings`]: the runtime's declared settings,
/// narrowed to what can actually be enabled right now, plus (for
/// speculative decoding specifically) the installed models a `Text`-schema
/// draft-model setting can validly reference — a fixed `Choice` schema
/// cannot express this list since it is relative to whichever model is
/// currently selected, not fixed at capability-declaration time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3SettingCapabilitiesView {
    pub settings: Vec<AdvancedSettingCapability>,
    pub draft_model_candidates: Vec<M3DraftModelCandidate>,
}

/// Whether at least one non-CPU accelerator is confirmed `Available` per the
/// Hardware Compatibility Matrix report. Shared by both the flash-attention
/// and mixed-precision gates: llama.cpp requires flash attention to quantize
/// the KV cache below f16, and flash attention itself needs real GPU
/// acceleration (see each gate's reason string for the user-facing detail).
fn gpu_backend_available(report: &M3HardwareCompatibilityReport) -> bool {
    report.accelerators.iter().any(|accelerator| {
        accelerator.kind != AcceleratorKind::Cpu
            && accelerator.status == M3AcceleratorStatus::Available
    })
}

/// `Some(reason)` when flash attention cannot be enabled on this machine
/// right now, `None` when it can. Shared by the UI-facing resolver and the
/// authoritative `set_runtime_config` enforcement so both always agree.
fn flash_attention_block_reason(report: &M3HardwareCompatibilityReport) -> Option<String> {
    if gpu_backend_available(report) {
        None
    } else {
        Some(
            "Flash attention needs a supported GPU backend (Metal, CUDA, ROCm, or Vulkan); \
             this machine's Hardware Compatibility report shows CPU only."
                .to_string(),
        )
    }
}

/// `Some(reason)` when quantized-KV-cache "mixed precision" cannot be
/// enabled on this machine right now, `None` when it can.
fn mixed_precision_block_reason(report: &M3HardwareCompatibilityReport) -> Option<String> {
    if gpu_backend_available(report) {
        None
    } else {
        Some(
            "Quantized KV cache (mixed precision) needs a supported GPU backend and flash \
             attention; this machine's Hardware Compatibility report shows CPU only."
                .to_string(),
        )
    }
}

/// Installed models that could serve as a speculative-decoding draft model
/// for `target`: same runtime (llama.cpp only — see the module doc comment
/// for why), a named (non-`Generic`) family that matches `target`'s, a
/// different asset than `target` itself, and a genuinely smaller estimated
/// footprint (a draft model only speeds up generation if it is cheaper to
/// run per token than the model it drafts for).
fn compatible_draft_models<'a>(
    target: &M3InstalledModelView,
    installed: &'a [M3InstalledModelView],
) -> Vec<&'a M3InstalledModelView> {
    if target.runtime != M3RuntimeKind::LlamaCpp {
        return Vec::new();
    }
    let target_family =
        ModelFamily::detect(&target.model_id, &target.display_name, &target.variant_id);
    if target_family == ModelFamily::Generic {
        return Vec::new();
    }
    installed
        .iter()
        .filter(|candidate| {
            candidate.asset_id != target.asset_id
                && candidate.runtime == M3RuntimeKind::LlamaCpp
                && candidate.estimated_ram_bytes < target.estimated_ram_bytes
                && ModelFamily::detect(
                    &candidate.model_id,
                    &candidate.display_name,
                    &candidate.variant_id,
                ) == target_family
        })
        .collect()
}

/// Narrows a runtime's declared `AdvancedSettingCapability` list down to
/// what the current hardware and (optionally) selected target model can
/// actually honor. Pure and deterministic — no I/O, no clock — so it is
/// trivially unit-testable with fixture reports/models. `target: None`
/// covers the "no model selected yet" case: hardware-only gates
/// (flash attention, mixed precision) still resolve correctly, while the
/// model-relative speculative-decoding gate reports "select a model" rather
/// than guessing.
pub fn gate_advanced_settings(
    capabilities: &[AdvancedSettingCapability],
    compatibility: &M3HardwareCompatibilityReport,
    target: Option<&M3InstalledModelView>,
    installed: &[M3InstalledModelView],
) -> M3SettingCapabilitiesView {
    let mut draft_model_candidates = Vec::new();
    let settings = capabilities
        .iter()
        .cloned()
        .map(|mut capability| {
            match capability.key.as_str() {
                "flash_attention" => {
                    if let Some(reason) = flash_attention_block_reason(compatibility) {
                        capability.supported = false;
                        capability.unsupported_reason = Some(reason);
                    }
                }
                "mixed_precision" => {
                    if let Some(reason) = mixed_precision_block_reason(compatibility) {
                        capability.supported = false;
                        capability.unsupported_reason = Some(reason);
                    }
                }
                "speculative_decoding_draft_model" => match target {
                    None => {
                        capability.supported = false;
                        capability.unsupported_reason = Some(
                            "Select a model to check for a compatible installed draft model."
                                .to_string(),
                        );
                    }
                    Some(target_model) => {
                        let candidates = compatible_draft_models(target_model, installed);
                        if candidates.is_empty() {
                            capability.supported = false;
                            capability.unsupported_reason = Some(format!(
                                "No compatible draft model installed. Install a smaller, \
                                 same-family model than {} for llama.cpp to enable speculative \
                                 decoding.",
                                target_model.display_name
                            ));
                        } else {
                            capability.supported = true;
                            capability.unsupported_reason = None;
                            draft_model_candidates.extend(candidates.iter().map(|candidate| {
                                M3DraftModelCandidate {
                                    model_id: candidate.model_id.clone(),
                                    display_name: candidate.display_name.clone(),
                                }
                            }));
                        }
                    }
                },
                _ => {}
            }
            capability
        })
        .collect();
    M3SettingCapabilitiesView {
        settings,
        draft_model_candidates,
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "runtimeType", rename_all = "snake_case")]
pub enum M3RuntimeStatusView {
    Adapter {
        status: RuntimeStatus,
        running_models: Vec<RunningModel>,
    },
    #[cfg(target_os = "macos")]
    Mlx { status: MlxRuntimeStatus },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "runtimeType", rename_all = "snake_case")]
pub enum M3RuntimeMetricsView {
    Adapter {
        status: RuntimeStatus,
        running_models: Vec<RunningModel>,
    },
    #[cfg(target_os = "macos")]
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

    /// Generates embeddings for a batch of text inputs. The default rejects
    /// with `Unsupported` — only engines that genuinely reach a backend
    /// capable of producing real vectors (see
    /// `OpenAiCompatibleM3InferenceEngine` in `m3_production.rs`) override
    /// this; nothing here ever fabricates a vector.
    fn embed<'a>(
        &'a self,
        request: &'a CanonicalEmbeddingRequest,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalEmbeddingResponse> {
        Box::pin(async move {
            Err(M3HubError::Unsupported(format!(
                "model {} does not have an embeddings-capable inference engine",
                request.model
            )))
        })
    }
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

    /// Generates embeddings through this runtime. The default rejects with
    /// `Unsupported` — see [`M3InferenceEngine::embed`]'s doc for why this
    /// stays an honest rejection rather than a fabricated vector wherever a
    /// driver does not override it (for example the managed MLX driver,
    /// which has no embeddings support today).
    fn embed<'a>(
        &'a self,
        request: &'a CanonicalEmbeddingRequest,
        _context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalEmbeddingResponse> {
        let runtime_id = self.descriptor().runtime_id;
        Box::pin(async move {
            Err(M3HubError::Unsupported(format!(
                "runtime {runtime_id} does not support embeddings generation for model {}",
                request.model
            )))
        })
    }
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
        let limits = RuntimeOperationLimits::with_timeout_ms(context.timeout_ms);
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
            // Ollama's daemon serves an OpenAI-compatible `/v1/embeddings`
            // endpoint alongside chat; the managed llama.cpp chat instance
            // is started without `--embeddings` and therefore cannot.
            can_embed: self.descriptor.kind == M3RuntimeKind::Ollama,
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

    fn embed<'a>(
        &'a self,
        request: &'a CanonicalEmbeddingRequest,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, CanonicalEmbeddingResponse> {
        self.inference.embed(request, context)
    }
}

#[cfg(target_os = "macos")]
pub struct MlxM3Driver {
    runtime_id: String,
    adapter: Arc<MlxRuntimeAdapter>,
    clock: Arc<dyn M3Clock>,
}

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
            can_embed: false,
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

/// Authenticated owner of an in-flight inference. This type is deliberately
/// crate-private and has no Serde implementation: public/Tauri request
/// envelopes cannot assert an already-authorized paired-token identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum M3RequestPrincipal {
    Internal,
    PairedToken(String),
}

struct M3ApiAuthorization {
    candidates: Option<AuthorizedBackendCandidates>,
    principal: M3RequestPrincipal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct M3InFlightInferenceBinding {
    pub(crate) runtime_id: String,
    pub(crate) model_id: String,
    pub(crate) scope: ApiScope,
    principal: M3RequestPrincipal,
    registration_id: Uuid,
}

struct M3InFlightInferenceEntry {
    binding: M3InFlightInferenceBinding,
    cancel_in_progress: bool,
    dispatch_finished: bool,
}

struct M3InFlightInferenceGuard<'a> {
    request_id: String,
    registration_id: Uuid,
    registry: &'a Mutex<BTreeMap<String, M3InFlightInferenceEntry>>,
}

impl Drop for M3InFlightInferenceGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.registry.lock() {
            let remove = match in_flight.get_mut(&self.request_id) {
                Some(entry) if entry.binding.registration_id == self.registration_id => {
                    if entry.cancel_in_progress {
                        entry.dispatch_finished = true;
                        false
                    } else {
                        true
                    }
                }
                _ => false,
            };
            if remove {
                in_flight.remove(&self.request_id);
            }
        }
    }
}

struct M3InFlightCancellationGuard<'a> {
    request_id: String,
    registration_id: Uuid,
    registry: &'a Mutex<BTreeMap<String, M3InFlightInferenceEntry>>,
}

impl Drop for M3InFlightCancellationGuard<'_> {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.registry.lock() {
            let remove = match in_flight.get_mut(&self.request_id) {
                Some(entry) if entry.binding.registration_id == self.registration_id => {
                    if entry.dispatch_finished {
                        true
                    } else {
                        entry.cancel_in_progress = false;
                        false
                    }
                }
                _ => false,
            };
            if remove {
                in_flight.remove(&self.request_id);
            }
        }
    }
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
    in_flight_inference: Mutex<BTreeMap<String, M3InFlightInferenceEntry>>,
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
            in_flight_inference: Mutex::new(BTreeMap::new()),
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

    /// Builds a compatibility matrix — one row per advertised route, per
    /// configured runtime, and (for capability-specific routes) per
    /// installed model. This is deliberately capability-derived rather than
    /// a live per-cell network probe: it reflects the same runtime/model
    /// capability state (`can_infer`, `can_embed`,
    /// `M3ModelCapabilities`) that already gates real requests, kept
    /// accurate by runtime reconciliation. Regressions in the actual wire
    /// behavior are caught by the `m3_compatibility_harness` integration
    /// test suite, which spins up the real HTTP server and makes real
    /// requests; this method is what the Runtime/API Hub UI renders.
    pub fn compatibility_matrix(&self) -> M3HubResult<M3CompatibilityMatrixReport> {
        let runtimes = self.list_runtimes()?;
        let installed = self.list_installed_models()?;
        let mut rows = Vec::new();
        for runtime in &runtimes {
            let backend = runtime.descriptor.api_backend;
            let runtime_id = runtime.descriptor.runtime_id.clone();
            rows.push(M3CompatibilityMatrixRow::new(
                "GET",
                "/v1/models",
                backend,
                &runtime_id,
                None,
                M3CompatibilityStatus::Pass,
                "runtime is registered and discoverable",
            ));
            rows.push(M3CompatibilityMatrixRow::new(
                "GET",
                "/api/tags",
                backend,
                &runtime_id,
                None,
                M3CompatibilityStatus::Pass,
                "native-Ollama listing reshapes the same discovery data as /v1/models",
            ));
            let infer_status = if runtime.can_infer {
                (
                    M3CompatibilityStatus::Pass,
                    "runtime driver supports inference".to_string(),
                )
            } else {
                (
                    M3CompatibilityStatus::Unsupported,
                    "runtime driver does not support inference".to_string(),
                )
            };
            for (method, route) in [
                ("POST", "/v1/chat/completions"),
                ("POST", "/v1/responses"),
                ("POST", "/v1/messages"),
                ("POST", "/api/chat"),
            ] {
                rows.push(M3CompatibilityMatrixRow::new(
                    method,
                    route,
                    backend,
                    &runtime_id,
                    None,
                    infer_status.0,
                    &infer_status.1,
                ));
            }
            rows.push(M3CompatibilityMatrixRow::new(
                "POST",
                "/v1/embeddings",
                backend,
                &runtime_id,
                None,
                if runtime.can_embed {
                    M3CompatibilityStatus::Pass
                } else {
                    M3CompatibilityStatus::Unsupported
                },
                if runtime.can_embed {
                    "runtime driver reaches a genuine embeddings endpoint"
                } else {
                    "runtime driver has no embeddings-capable endpoint configured"
                },
            ));
            for model in installed
                .iter()
                .filter(|model| model.runtime == runtime.descriptor.kind)
            {
                rows.push(M3CompatibilityMatrixRow::new(
                    "POST",
                    "/v1/chat/completions (tool calls)",
                    backend,
                    &runtime_id,
                    Some(&model.model_id),
                    capability_status(model.capabilities.tool_calling),
                    if model.capabilities.tool_calling {
                        "model advertises tool calling"
                    } else {
                        "model does not advertise tool calling"
                    },
                ));
                rows.push(M3CompatibilityMatrixRow::new(
                    "POST",
                    "/v1/chat/completions (json schema)",
                    backend,
                    &runtime_id,
                    Some(&model.model_id),
                    capability_status(model.capabilities.structured_output),
                    if model.capabilities.structured_output {
                        "model advertises structured output"
                    } else {
                        "model does not advertise structured output"
                    },
                ));
                let embeddings_ok = runtime.can_embed && model.capabilities.embeddings;
                rows.push(M3CompatibilityMatrixRow::new(
                    "POST",
                    "/v1/embeddings",
                    backend,
                    &runtime_id,
                    Some(&model.model_id),
                    capability_status(embeddings_ok),
                    if embeddings_ok {
                        "model advertises embeddings on an embeddings-capable runtime"
                    } else if !runtime.can_embed {
                        "runtime driver has no embeddings-capable endpoint configured"
                    } else {
                        "model does not advertise embeddings"
                    },
                ));
            }
        }
        Ok(M3CompatibilityMatrixReport {
            generated_at_ms: self.clock.now_ms()?,
            rows,
        })
    }

    pub fn hardware_snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        let snapshot = self.hardware.snapshot()?;
        snapshot.profile().map_err(runtime_error)?;
        Ok(snapshot)
    }

    pub fn hardware_profile(&self) -> M3HubResult<HardwareProfile> {
        self.hardware_snapshot()?.profile().map_err(runtime_error)
    }

    /// Hardware Compatibility Matrix / "Driver Doctor" report. Must be safe
    /// to call before any model download, model load, or runtime install so
    /// the UI can show a concrete compatibility report first.
    pub fn hardware_compatibility_report(&self) -> M3HubResult<M3HardwareCompatibilityReport> {
        self.hardware.compatibility_report()
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
            for mut entry in entries {
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
                    // Provenance is stamped locally at retrieval time; a
                    // remote source's own claim about this is never trusted.
                    entry.catalog_retrieved_at_ms = Some(self.clock.now_ms()?);
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

    /// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
    /// 14): flags an installed model as outdated when its catalog has a
    /// different revision available *and* it has gone unrefreshed for a long
    /// time — reusing `search_catalog` (the same mechanism the "Find
    /// updates" button already drives) for the "different revision" half
    /// rather than inventing a second catalog-freshness signal. Compares
    /// only against catalog entries from the same source as the installed
    /// active version, so an unrelated source's differently-quantized or
    /// differently-sourced listing of the same model id is never mistaken
    /// for "the same thing, but newer". Returns `Ok(None)` for an
    /// up-to-date, recently-installed, or catalog-absent model — this is a
    /// diagnostic signal, never an error.
    pub async fn model_staleness_check(
        &self,
        asset_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<Option<crate::model_retirement::LocalModelStalenessWarning>> {
        context.preflight("model staleness check")?;
        validate_identifier(asset_id, "assetId")?;
        let installed = self.list_installed_models()?;
        let model = installed
            .iter()
            .find(|candidate| candidate.asset_id == asset_id)
            .ok_or_else(|| M3HubError::NotFound(format!("model {asset_id}")))?;
        let active = model
            .versions
            .iter()
            .find(|version| version.version_key == model.active_version_key)
            .ok_or_else(|| M3HubError::State("active model version is missing".to_string()))?;
        let matches = self
            .search_catalog(&model.model_id, self.config.max_catalog_results, context)
            .await?;
        let latest = matches.iter().find(|candidate| {
            candidate.model.model_id == model.model_id
                && candidate.model.variant_id == model.variant_id
                && candidate.model.source_id == active.source_id
        });
        let now_ms = self.clock.now_ms()?;
        Ok(latest.and_then(|candidate| {
            crate::model_retirement::check_local_model_staleness(
                &model.asset_id,
                &active.revision,
                active.installed_at_ms,
                &candidate.model.revision,
                &candidate.model.display_name,
                now_ms,
            )
        }))
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

        let partial_path = self
            .downloads_root
            .join(format!("{asset_key}{DOWNLOAD_SUFFIX}"));
        let resume_path = self
            .downloads_root
            .join(format!("{asset_key}{RESUME_SUFFIX}"));

        // Layer reuse: if a byte-identical, independently re-verified payload
        // is already installed under a different asset/version, materialize
        // it locally without a network transfer instead of re-downloading
        // content we already have. A mismatched or corrupt candidate is
        // never trusted here (see `find_reusable_payload`), so this always
        // falls back to a real download when reuse cannot be proven safe.
        let reusable_payload = {
            let _guard = lock(&self.state_lock)?;
            let state = load_hub_state(&self.state_root, &self.models_root)?;
            find_reusable_payload(
                &state,
                &self.models_root,
                &request.model.sha256,
                request.model.size_bytes,
            )?
        };

        if let Some(existing_payload) = reusable_payload {
            context.preflight("reuse verified model payload")?;
            let available = self.storage_status()?.available_for_models_bytes;
            if request.model.size_bytes > available {
                return Err(M3HubError::Storage {
                    required: request.model.size_bytes,
                    available,
                });
            }
            remove_owned_file(&partial_path)?;
            remove_owned_file(&resume_path)?;
            link_or_copy_owned(&existing_payload, &partial_path)?;
        } else {
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
                    (request.model.size_bytes - offset)
                        .min(self.config.download_chunk_bytes as u64),
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
        }
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
            // A fresh (or replaced) version always starts unverified, even
            // when it replaces an existing entry for the same version key:
            // that replace path only fires when the catalog re-declares
            // non-content-addressed fields (e.g. a corrected `projector`
            // reference) for what is otherwise byte-identical content, and a
            // stale verification must not silently carry over to a
            // potentially different projector declaration.
            projector_verified_sha256: None,
            projector_verified_at_ms: None,
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

    /// Verifies a candidate local file against an installed model version's
    /// declared projector reference (kind/sha256/size), promoting that
    /// version's projector from merely "declared" to genuine `Verified`
    /// evidence on success (ROADMAP Phase 8 item 12). This intentionally
    /// does not download anything itself: unlike a model's own weights,
    /// `M3ProjectorRef` carries no download URL — a user (or a future PR
    /// that adds a real fetch path) supplies the file, and this is the real,
    /// digest-checked promotion step, mirroring the same `sha256_file`/
    /// `constant_time_eq` integrity check a model's own weights go through
    /// in `download_model`. Never marks anything verified on a digest
    /// mismatch — this returns `M3HubError::Integrity` instead, the same
    /// error a corrupted model download produces.
    pub async fn verify_projector(
        &self,
        request: &M3VerifyProjectorRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledModelView> {
        validate_identifier(&request.asset_id, "assetId")?;
        validate_sha256(&request.version_key, "versionKey")?;
        context.preflight("verify projector")?;
        let _mutation = self.mutation_lock.lock().await;
        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_hub_state(&self.state_root, &self.models_root)?
        };
        let model_index = state
            .models
            .iter()
            .position(|model| model.asset_id == request.asset_id)
            .ok_or_else(|| M3HubError::NotFound(request.asset_id.clone()))?;
        let version_index = state.models[model_index]
            .versions
            .iter()
            .position(|version| version.version_key == request.version_key)
            .ok_or_else(|| M3HubError::NotFound(request.version_key.clone()))?;
        let projector = state.models[model_index].versions[version_index]
            .model
            .projector
            .clone()
            .ok_or_else(|| {
                M3HubError::NotFound("this model version declares no projector".to_string())
            })?;
        let actual_digest = sha256_file(&request.candidate_path, projector.size_bytes)?;
        if !constant_time_eq(actual_digest.as_bytes(), projector.sha256.as_bytes()) {
            return Err(M3HubError::Integrity {
                expected: projector.sha256.clone(),
                actual: actual_digest,
            });
        }
        let verified_at_ms = self.clock.now_ms()?;
        state.models[model_index].versions[version_index].projector_verified_sha256 =
            Some(projector.sha256.clone());
        state.models[model_index].versions[version_index].projector_verified_at_ms =
            Some(verified_at_ms);
        let view = stored_model_view(&state.models[model_index], &self.models_root)?;
        {
            let _guard = lock(&self.state_lock)?;
            save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        Ok(view)
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
        self.enforce_draft_model_gate(&model, &settings)?;
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

    /// Authoritative, server-side half of the speculative-decoding gate: the
    /// UI-facing `resolve_setting_capabilities` only decides what to *show*
    /// as enabled; this decides what actually gets to run. The persisted
    /// runtime config is model-agnostic (one config applies to whatever
    /// gets loaded next), so the draft-model choice can only be checked
    /// against a *specific* target once that target is known — which is
    /// exactly here, at load time, mirroring how `RuntimeAdapterM3Driver::load`
    /// already does its own load-time-only "has this runtime reconciled
    /// this model" check beyond the low-level adapter's static validation.
    fn enforce_draft_model_gate(
        &self,
        model: &M3ResolvedModel,
        settings: &BTreeMap<String, SettingValue>,
    ) -> M3HubResult<()> {
        let Some(SettingValue::Text { value }) = settings.get("speculative_decoding_draft_model")
        else {
            return Ok(());
        };
        if value.is_empty() {
            return Ok(());
        }
        if model.runtime != M3RuntimeKind::LlamaCpp {
            return Err(M3HubError::Unsupported(
                "Speculative decoding is only available for llama.cpp".to_string(),
            ));
        }
        let installed = self.list_installed_models()?;
        let target_view = installed
            .iter()
            .find(|entry| entry.asset_id == model.asset_id)
            .ok_or_else(|| M3HubError::NotFound(format!("model {}", model.asset_id)))?;
        let candidates = compatible_draft_models(target_view, &installed);
        if candidates
            .iter()
            .any(|candidate| candidate.model_id == *value)
        {
            Ok(())
        } else {
            Err(M3HubError::Unsupported(format!(
                "{value} is not a compatible installed draft model for {}",
                target_view.display_name
            )))
        }
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
        self.enforce_hardware_setting_gates(&request.values)?;
        let _guard = lock(&self.state_lock)?;
        let mut state = load_hub_state(&self.state_root, &self.models_root)?;
        state
            .runtime_configs
            .insert(request.runtime_id.clone(), request.values.clone());
        save_next_hub_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        Ok(request.values.clone())
    }

    /// Authoritative, server-side half of the flash-attention/mixed-precision
    /// gates: hardware support is machine-level (not model-relative), so —
    /// unlike the draft-model gate, which can only be checked once a target
    /// model is known at load time — this can and does run right at
    /// config-save time. The UI-facing `resolve_setting_capabilities` shows
    /// the same gate ahead of time so this should only ever reject a request
    /// that bypassed the UI (a direct IPC call, a stale client).
    fn enforce_hardware_setting_gates(
        &self,
        values: &BTreeMap<String, SettingValue>,
    ) -> M3HubResult<()> {
        let flash_wants_on = matches!(values.get("flash_attention"), Some(SettingValue::Choice { value }) if value == "on");
        let mixed_wants_non_f16 = matches!(values.get("mixed_precision"), Some(SettingValue::Choice { value }) if value != "f16");
        if !flash_wants_on && !mixed_wants_non_f16 {
            return Ok(());
        }
        let compatibility = self.hardware_compatibility_report()?;
        if flash_wants_on {
            if let Some(reason) = flash_attention_block_reason(&compatibility) {
                return Err(M3HubError::Unsupported(reason));
            }
        }
        if mixed_wants_non_f16 {
            if let Some(reason) = mixed_precision_block_reason(&compatibility) {
                return Err(M3HubError::Unsupported(reason));
            }
        }
        Ok(())
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

    /// UI-facing gating resolver: narrows `runtime_id`'s declared advanced
    /// settings down to what the current hardware and (if `asset_id` is
    /// given) selected target model can actually honor, via
    /// [`gate_advanced_settings`]. `asset_id: None` still resolves the
    /// hardware-only gates (flash attention, mixed precision) correctly —
    /// only the model-relative speculative-decoding gate needs a target.
    pub fn resolve_setting_capabilities(
        &self,
        runtime_id: &str,
        asset_id: Option<&str>,
    ) -> M3HubResult<M3SettingCapabilitiesView> {
        validate_identifier(runtime_id, "runtimeId")?;
        let runtime = self.runtime(runtime_id)?;
        let capabilities = runtime.capabilities();
        let compatibility = self.hardware_compatibility_report()?;
        let installed = self.list_installed_models()?;
        let target = match asset_id {
            Some(asset_id) => {
                validate_identifier(asset_id, "assetId")?;
                Some(
                    installed
                        .iter()
                        .find(|model| model.asset_id == asset_id)
                        .ok_or_else(|| M3HubError::NotFound(format!("model {asset_id}")))?,
                )
            }
            None => None,
        };
        Ok(gate_advanced_settings(
            &capabilities.settings,
            &compatibility,
            target,
            &installed,
        ))
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

    /// Validates only the paired credential at a transport edge. This is
    /// deliberately read-only and quota-free; the exact buffered byte count
    /// is charged once by `authorize_external_staged_request` below.
    pub fn preflight_external_credential(
        &self,
        bearer_token: &str,
        remote_address: &str,
        now_ms: u64,
    ) -> M3HubResult<()> {
        self.lan_controller()?
            .preflight_credential(&CredentialPreflightRequest {
                bearer_token: bearer_token.to_string(),
                remote_address: remote_address.to_string(),
                now_ms,
            })
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

    /// Performs the full external gate before model/runtime discovery.  The
    /// returned backend set is both an authorization receipt and the hard
    /// boundary for subsequent catalog resolution; the caller must not call
    /// `authorize_external_operation` again for the same request.
    pub fn authorize_external_backend_candidates(
        &self,
        request: &M3ExternalBackendCandidateAuthorization,
    ) -> M3HubResult<AuthorizedBackendCandidates> {
        self.lan_controller()?
            .authorize_backend_candidates(&BackendCandidateAuthorizationRequest {
                bearer_token: request.bearer_token.clone(),
                scope: request.scope,
                model_id: request.model_id.clone(),
                input_bytes: request.input_bytes,
                remote_address: request.remote_address.clone(),
                destructive_confirmation: request.destructive_confirmation.clone(),
                deferred_destructive_resource_id: request.deferred_destructive_resource_id.clone(),
                now_ms: request.now_ms,
            })
            .map_err(M3HubError::from)
    }

    /// Performs the single quota-bearing gate before an HTTP request envelope
    /// is parsed. The returned candidate receipt must be narrowed after parse;
    /// no downstream operation may authorize or debit this request again.
    pub fn authorize_external_staged_request(
        &self,
        request: &M3ExternalStagedAuthorization,
    ) -> M3HubResult<AuthorizedStagedRequest> {
        self.lan_controller()?
            .authorize_staged_request(&StagedAuthorizationRequest {
                bearer_token: request.bearer_token.clone(),
                scope: request.scope,
                input_bytes: request.input_bytes,
                remote_address: request.remote_address.clone(),
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
                #[cfg(target_os = "macos")]
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum M3ApiCaller {
    Internal,
    External {
        #[serde(skip_serializing)]
        bearer_token: String,
        remote_address: String,
    },
}

impl fmt::Debug for M3ApiCaller {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Internal => formatter.write_str("Internal"),
            Self::External { remote_address, .. } => formatter
                .debug_struct("External")
                .field("bearer_token", &"[REDACTED]")
                .field("remote_address", remote_address)
                .finish(),
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
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

#[derive(Clone, PartialEq, Eq)]
pub struct M3ExternalBackendCandidateAuthorization {
    pub bearer_token: String,
    pub scope: ApiScope,
    pub model_id: Option<String>,
    pub input_bytes: u64,
    pub remote_address: String,
    pub destructive_confirmation: Option<String>,
    pub deferred_destructive_resource_id: Option<String>,
    pub now_ms: u64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct M3ExternalStagedAuthorization {
    pub bearer_token: String,
    pub scope: Option<ApiScope>,
    pub input_bytes: u64,
    pub remote_address: String,
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

/// Dispatch envelope for `POST /v1/embeddings`. Distinct from
/// [`M3ApiDispatchRequest`] because embeddings have no
/// [`CompatibilityProtocol`] of their own — see
/// [`translate_embeddings_request`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3EmbeddingDispatchRequest {
    pub runtime_id: String,
    pub request_id: String,
    pub body: Vec<u8>,
    pub caller: M3ApiCaller,
    pub now_ms: u64,
}

/// Dispatch envelope for the Ollama-native `POST /api/chat`. Reuses the
/// `ChatCompletions` scope for authorization since it is the same
/// underlying operation as `/v1/chat/completions`, just a different wire
/// format — not a new, less-guarded route class.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3OllamaChatDispatchRequest {
    pub runtime_id: String,
    pub request_id: String,
    pub body: Vec<u8>,
    pub caller: M3ApiCaller,
    pub now_ms: u64,
}

/// Result of dispatching the Ollama-native `/api/chat` endpoint: the
/// encoded body plus whether the request asked for `stream` framing (the
/// HTTP layer uses this only to pick a Content-Type; see
/// [`translate_ollama_chat_request`]'s doc for the streaming limitation).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3OllamaChatDispatchResponse {
    pub body: Value,
    pub stream_requested: bool,
}

impl M3RuntimeHub {
    /// Returns the authoritative metadata for an active inference request.
    /// Transport callers may use this only after their credential/quota gate
    /// has succeeded; cancellation authorization must be narrowed against
    /// this stored binding rather than untrusted fields in the cancel body.
    pub(crate) fn in_flight_inference_binding(
        &self,
        request_id: &str,
    ) -> M3HubResult<M3InFlightInferenceBinding> {
        validate_identifier(request_id, "requestId")?;
        lock(&self.in_flight_inference)?
            .get(request_id)
            .map(|entry| entry.binding.clone())
            .ok_or_else(|| M3HubError::NotFound("in-flight request".to_string()))
    }

    fn register_in_flight_inference(
        &self,
        request_id: &str,
        runtime_id: &str,
        model_id: &str,
        scope: ApiScope,
        principal: &M3RequestPrincipal,
    ) -> M3HubResult<M3InFlightInferenceGuard<'_>> {
        validate_identifier(request_id, "requestId")?;
        validate_identifier(runtime_id, "runtimeId")?;
        validate_identifier(model_id, "modelId")?;
        let mut in_flight = lock(&self.in_flight_inference)?;
        if in_flight.contains_key(request_id) {
            return Err(M3HubError::Conflict(format!(
                "requestId {request_id} is already in flight"
            )));
        }
        let registration_id = Uuid::new_v4();
        in_flight.insert(
            request_id.to_string(),
            M3InFlightInferenceEntry {
                binding: M3InFlightInferenceBinding {
                    runtime_id: runtime_id.to_string(),
                    model_id: model_id.to_string(),
                    scope,
                    principal: principal.clone(),
                    registration_id,
                },
                cancel_in_progress: false,
                dispatch_finished: false,
            },
        );
        drop(in_flight);
        Ok(M3InFlightInferenceGuard {
            request_id: request_id.to_string(),
            registration_id,
            registry: &self.in_flight_inference,
        })
    }

    fn begin_in_flight_cancellation(
        &self,
        request_id: &str,
        registration_id: Uuid,
    ) -> M3HubResult<M3InFlightCancellationGuard<'_>> {
        let mut in_flight = lock(&self.in_flight_inference)?;
        let entry = in_flight
            .get_mut(request_id)
            .filter(|entry| entry.binding.registration_id == registration_id)
            .ok_or_else(|| M3HubError::NotFound("in-flight request".to_string()))?;
        if entry.cancel_in_progress {
            return Err(M3HubError::Conflict(format!(
                "cancellation for requestId {request_id} is already in progress"
            )));
        }
        entry.cancel_in_progress = true;
        drop(in_flight);
        Ok(M3InFlightCancellationGuard {
            request_id: request_id.to_string(),
            registration_id,
            registry: &self.in_flight_inference,
        })
    }

    pub async fn dispatch_api(
        &self,
        request: &M3ApiDispatchRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3ApiDispatchResponse> {
        self.dispatch_api_with_principal(request, None, context)
            .await
    }

    pub(crate) async fn dispatch_pre_authorized_api(
        &self,
        request: &M3ApiDispatchRequest,
        principal: M3RequestPrincipal,
        context: &M3OperationContext,
    ) -> M3HubResult<M3ApiDispatchResponse> {
        self.dispatch_api_with_principal(request, Some(principal), context)
            .await
    }

    async fn dispatch_api_with_principal(
        &self,
        request: &M3ApiDispatchRequest,
        trusted_principal: Option<M3RequestPrincipal>,
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
        let authorization = self.authorize_api_candidates(
            &request.caller,
            protocol_scope(request.protocol),
            &canonical.model,
            request.body.len() as u64,
            request.now_ms,
            trusted_principal.as_ref(),
        )?;
        let runtime = self
            .runtime_after_authorization(&request.runtime_id, authorization.candidates.as_ref())?;
        let runtime_id = runtime.descriptor().runtime_id;
        let _in_flight = self.register_in_flight_inference(
            &request.request_id,
            &runtime_id,
            &canonical.model,
            protocol_scope(request.protocol),
            &authorization.principal,
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
        self.dispatch_api_stream_with_principal(request, sink, None, None, context)
            .await
    }

    pub(crate) async fn dispatch_pre_authorized_api_stream(
        &self,
        request: &M3ApiDispatchRequest,
        sink: &mut dyn M3ProtocolFrameSink,
        principal: M3RequestPrincipal,
        ready: tokio::sync::oneshot::Sender<M3HubResult<()>>,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        self.dispatch_api_stream_with_principal(
            request,
            sink,
            Some(principal),
            Some(ready),
            context,
        )
        .await
    }

    async fn dispatch_api_stream_with_principal(
        &self,
        request: &M3ApiDispatchRequest,
        sink: &mut dyn M3ProtocolFrameSink,
        trusted_principal: Option<M3RequestPrincipal>,
        mut ready: Option<tokio::sync::oneshot::Sender<M3HubResult<()>>>,
        context: &M3OperationContext,
    ) -> M3HubResult<()> {
        let prepared: M3HubResult<_> = (|| {
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
            let authorization = self.authorize_api_candidates(
                &request.caller,
                protocol_scope(request.protocol),
                &canonical.model,
                request.body.len() as u64,
                request.now_ms,
                trusted_principal.as_ref(),
            )?;
            let runtime = self.runtime_after_authorization(
                &request.runtime_id,
                authorization.candidates.as_ref(),
            )?;
            let runtime_id = runtime.descriptor().runtime_id;
            let in_flight = self.register_in_flight_inference(
                &request.request_id,
                &runtime_id,
                &canonical.model,
                protocol_scope(request.protocol),
                &authorization.principal,
            )?;
            Ok((canonical, runtime, in_flight))
        })();
        let (canonical, runtime, _in_flight) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Some(ready) = ready.take() {
                    let _ = ready.send(Err(error));
                    return Ok(());
                }
                return Err(error);
            }
        };
        // The HTTP layer waits for this signal before it commits SSE 200
        // headers. Therefore an immediate cancel always observes the binding,
        // and a duplicate active requestId is an HTTP conflict rather than a
        // late error frame inside an already-successful stream.
        if let Some(ready) = ready.take() {
            let _ = ready.send(Ok(()));
        }
        let mut encoding = ProtocolEncodingSink {
            protocol: request.protocol,
            downstream: sink,
        };
        runtime.stream(&canonical, &mut encoding, context).await
    }

    /// Dispatches `POST /v1/embeddings`: translates the request, resolves
    /// the runtime, authorizes with the `Embeddings` scope, and calls the
    /// runtime driver's `embed()` — which honestly rejects with
    /// `Unsupported` unless it genuinely reaches a backend capable of
    /// producing real vectors.
    pub async fn dispatch_embeddings(
        &self,
        request: &M3EmbeddingDispatchRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3ApiDispatchResponse> {
        context.preflight("dispatch embeddings request")?;
        let canonical = translate_embeddings_request(&request.request_id, &request.body)?;
        let authorization = self.authorize_api_candidates(
            &request.caller,
            ApiScope::Embeddings,
            &canonical.model,
            request.body.len() as u64,
            request.now_ms,
            None,
        )?;
        let runtime = self
            .runtime_after_authorization(&request.runtime_id, authorization.candidates.as_ref())?;
        let response = runtime.embed(&canonical, context).await?;
        if response.model != canonical.model {
            return Err(M3HubError::Runtime(
                "embeddings driver returned a response for another model".to_string(),
            ));
        }
        Ok(M3ApiDispatchResponse {
            status: 200,
            body: encode_embeddings_response(&response)?,
        })
    }

    /// Dispatches the Ollama-native `POST /api/chat`: translates the
    /// request into the same canonical inference representation used by
    /// `/v1/chat/completions`, authorizes with the `ChatCompletions` scope
    /// (same operation, different wire format), calls the resolved
    /// runtime's `complete()` (always non-streaming — see
    /// [`M3OllamaChatDispatchResponse`]'s doc), and encodes an
    /// Ollama-native response with a real measured `total_duration`.
    pub async fn dispatch_ollama_chat(
        &self,
        request: &M3OllamaChatDispatchRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3OllamaChatDispatchResponse> {
        self.dispatch_ollama_chat_with_principal(request, None, context)
            .await
    }

    pub(crate) async fn dispatch_pre_authorized_ollama_chat(
        &self,
        request: &M3OllamaChatDispatchRequest,
        principal: M3RequestPrincipal,
        context: &M3OperationContext,
    ) -> M3HubResult<M3OllamaChatDispatchResponse> {
        self.dispatch_ollama_chat_with_principal(request, Some(principal), context)
            .await
    }

    async fn dispatch_ollama_chat_with_principal(
        &self,
        request: &M3OllamaChatDispatchRequest,
        trusted_principal: Option<M3RequestPrincipal>,
        context: &M3OperationContext,
    ) -> M3HubResult<M3OllamaChatDispatchResponse> {
        context.preflight("dispatch ollama-native chat request")?;
        let (canonical, stream_requested) =
            translate_ollama_chat_request(&request.request_id, &request.body)?;
        let authorization = self.authorize_api_candidates(
            &request.caller,
            ApiScope::ChatCompletions,
            &canonical.model,
            request.body.len() as u64,
            request.now_ms,
            trusted_principal.as_ref(),
        )?;
        let runtime = self
            .runtime_after_authorization(&request.runtime_id, authorization.candidates.as_ref())?;
        let runtime_id = runtime.descriptor().runtime_id;
        let _in_flight = self.register_in_flight_inference(
            &request.request_id,
            &runtime_id,
            &canonical.model,
            ApiScope::ChatCompletions,
            &authorization.principal,
        )?;
        let started_at = std::time::Instant::now();
        let response = runtime.complete(&canonical, context).await?;
        let total_duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
        if response.model != canonical.model {
            return Err(M3HubError::Runtime(
                "inference driver returned a response for another model".to_string(),
            ));
        }
        Ok(M3OllamaChatDispatchResponse {
            body: encode_ollama_chat_response(&response, total_duration_ns)?,
            stream_requested,
        })
    }

    pub async fn cancel_inference(
        &self,
        request: &M3CancelInferenceRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<bool> {
        self.cancel_inference_with_principal(request, None, None, context)
            .await
    }

    pub(crate) async fn cancel_pre_authorized_inference(
        &self,
        request: &M3CancelInferenceRequest,
        binding: M3InFlightInferenceBinding,
        principal: M3RequestPrincipal,
        context: &M3OperationContext,
    ) -> M3HubResult<bool> {
        self.cancel_inference_with_principal(request, Some(binding), Some(principal), context)
            .await
    }

    async fn cancel_inference_with_principal(
        &self,
        request: &M3CancelInferenceRequest,
        trusted_binding: Option<M3InFlightInferenceBinding>,
        trusted_principal: Option<M3RequestPrincipal>,
        context: &M3OperationContext,
    ) -> M3HubResult<bool> {
        validate_identifier(&request.request_id, "requestId")?;
        validate_identifier(&request.model_id, "modelId")?;
        if trusted_principal.is_none() {
            if let M3ApiCaller::External {
                bearer_token,
                remote_address,
            } = &request.caller
            {
                // Keep invalid credentials from probing the in-flight registry.
                // This preflight is read-only and quota-free; authorization below
                // performs the request's single quota-bearing transaction.
                self.preflight_external_credential(bearer_token, remote_address, request.now_ms)?;
            }
        }
        let binding = match trusted_binding {
            Some(binding) => binding,
            None => self.in_flight_inference_binding(&request.request_id)?,
        };
        let authorization = match self.authorize_api_candidates(
            &request.caller,
            binding.scope,
            &binding.model_id,
            0,
            request.now_ms,
            trusted_principal.as_ref(),
        ) {
            Ok(authorization) => authorization,
            Err(M3HubError::Forbidden(_) | M3HubError::NotFound(_)) => {
                return Err(M3HubError::NotFound("in-flight request".to_string()))
            }
            Err(error) => return Err(error),
        };
        let runtime = match self
            .runtime_after_authorization(&binding.runtime_id, authorization.candidates.as_ref())
        {
            Ok(runtime) => runtime,
            Err(M3HubError::Forbidden(_) | M3HubError::NotFound(_)) => {
                return Err(M3HubError::NotFound("in-flight request".to_string()))
            }
            Err(error) => return Err(error),
        };
        if request.runtime_id != binding.runtime_id
            || request.model_id != binding.model_id
            || protocol_scope(request.protocol) != binding.scope
            || authorization.principal != binding.principal
        {
            return Err(M3HubError::NotFound("in-flight request".to_string()));
        }
        let _cancellation =
            self.begin_in_flight_cancellation(&request.request_id, binding.registration_id)?;
        runtime.cancel(&request.request_id, context).await
    }

    fn authorize_api_candidates(
        &self,
        caller: &M3ApiCaller,
        scope: ApiScope,
        model_id: &str,
        input_bytes: u64,
        now_ms: u64,
        trusted_principal: Option<&M3RequestPrincipal>,
    ) -> M3HubResult<M3ApiAuthorization> {
        if let Some(principal) = trusted_principal {
            if !matches!(caller, M3ApiCaller::Internal) {
                return Err(M3HubError::State(
                    "pre-authorized dispatch must use an internal request envelope".to_string(),
                ));
            }
            return Ok(M3ApiAuthorization {
                candidates: None,
                principal: principal.clone(),
            });
        }
        match caller {
            M3ApiCaller::Internal => Ok(M3ApiAuthorization {
                candidates: None,
                principal: M3RequestPrincipal::Internal,
            }),
            M3ApiCaller::External {
                bearer_token,
                remote_address,
            } => {
                let receipt = self.lan_controller()?.authorize_backend_candidates(
                    &BackendCandidateAuthorizationRequest {
                        bearer_token: bearer_token.clone(),
                        scope,
                        model_id: Some(model_id.to_string()),
                        input_bytes,
                        remote_address: remote_address.clone(),
                        destructive_confirmation: None,
                        deferred_destructive_resource_id: None,
                        now_ms,
                    },
                )?;
                let principal = M3RequestPrincipal::PairedToken(receipt.token_id.clone());
                Ok(M3ApiAuthorization {
                    candidates: Some(receipt),
                    principal,
                })
            }
        }
    }

    /// Streams one generation purely to time it, and returns only the timings.
    ///
    /// Deliberately not routed through `dispatch_api_stream`: that path exists to
    /// serve an external API caller, so it translates a wire protocol, authorizes
    /// a principal against a scope, and debits a quota — none of which a local
    /// measurement of the user's own model is. Going through it would also put the
    /// protocol encoder between the runtime and the clock, so time-to-first-token
    /// would be stamped on an SSE frame rather than on the canonical text delta
    /// that produced it.
    ///
    /// A driver error is recorded on the sample rather than discarding the repeat:
    /// a stream that failed after emitting text still measured a real
    /// time-to-first-token, and `summarize` excludes errored samples from the
    /// statistics anyway.
    pub async fn benchmark_stream_once(
        &self,
        runtime_id: &str,
        request: &CanonicalInferenceRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<crate::benchmark::SampleTimings> {
        context.preflight("benchmark one generation")?;
        let runtime = self.runtime(runtime_id)?;
        let mut sink = crate::benchmark::TimingSink::started_now();
        let outcome = runtime.stream(request, &mut sink, context).await;
        let mut timings = sink.finish();
        if let Err(error) = outcome {
            timings.record_error(error.to_string());
        }
        Ok(timings)
    }

    /// The OS pid of the process hosting `runtime_id`, or `None` when there is no
    /// local process to sample — a remote OpenAI-compatible endpoint has none, and
    /// neither does a managed runtime that is not currently running.
    pub async fn benchmark_runtime_pid(
        &self,
        runtime_id: &str,
        context: &M3OperationContext,
    ) -> M3HubResult<Option<i64>> {
        let runtime = self.runtime(runtime_id)?;
        let os_pid = match runtime.status(context).await? {
            M3RuntimeStatusView::Adapter { status, .. } => {
                status.process.as_ref().and_then(|handle| handle.os_pid)
            }
            #[cfg(target_os = "macos")]
            M3RuntimeStatusView::Mlx { status } => match status {
                MlxRuntimeStatus::Running { handle, .. } => handle.os_pid,
                _ => None,
            },
        };
        Ok(os_pid.map(i64::from))
    }

    fn runtime_after_authorization(
        &self,
        runtime_id: &str,
        authorization: Option<&AuthorizedBackendCandidates>,
    ) -> M3HubResult<Arc<dyn M3RuntimeDriver>> {
        // Authentication, scope/model checks, and quota debit have already
        // happened for external callers before this first runtime lookup.
        let runtime = self.runtime(runtime_id)?;
        if authorization
            .is_some_and(|receipt| !receipt.backends.contains(&runtime.descriptor().api_backend))
        {
            // Do not disclose whether the named runtime exists to a token that
            // cannot use its backend.
            return Err(M3HubError::NotFound("authorized runtime".to_string()));
        }
        Ok(runtime)
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
                    // MLX reports no prompt-cache reuse, so `None` — see
                    // `CanonicalUsage::cached_input_tokens` on why not zero.
                    cached_input_tokens: None,
                },
            })
            .map_err(stream_sink_error)?;
        self.completed = true;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
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
                            cached_input_tokens: None,
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

#[cfg(any(target_os = "macos", test))]
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

#[cfg(any(target_os = "macos", test))]
struct CanonicalToolAccumulator {
    call_id: String,
    name: String,
    arguments: String,
    ended: bool,
}

#[cfg(any(target_os = "macos", test))]
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

#[cfg(any(target_os = "macos", test))]
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
            if !request_offers_tool(request, &tool.name) {
                return Err(M3HubError::Runtime(format!(
                    "stream called tool \"{}\" that was not offered in this request",
                    tool.name
                )));
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

/// `pub(crate)` (rather than private) so both `m3_production.rs`'s tests and
/// `chat_template_lab.rs` (ROADMAP Phase 8 item 8) can exercise the real MLX
/// flattening path without duplicating it: `chat_template_lab.rs` reuses it
/// directly (not mocked) to validate the MLX driver's tool-call round-trip
/// alongside the OpenAI-compatible Ollama/llama.cpp path, and its own vision
/// fixture (ROADMAP item 12's prior art) exercises the same function.
#[cfg(target_os = "macos")]
pub(crate) fn canonical_message_to_mlx(message: &CanonicalMessage) -> M3HubResult<MlxMessage> {
    let role = match message.role {
        CanonicalRole::System => "system",
        CanonicalRole::User => "user",
        CanonicalRole::Assistant => "assistant",
        CanonicalRole::Tool => "tool",
    };
    let mut parts = Vec::new();
    // Image content blocks (ROADMAP Phase 8 item 12) carry no native slot in
    // `MlxMessage.text` — its wire type has a dedicated `images` list
    // instead, so a data URI is appended there rather than flattened into
    // the joined text like tool_use/tool_result JSON is.
    let mut images = Vec::new();
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
            CanonicalContent::Image {
                mime_type,
                data_base64,
            } => images.push(CanonicalContent::image_data_url(mime_type, data_base64)),
        }
    }
    Ok(MlxMessage {
        role: role.to_string(),
        text: parts.join("\n"),
        images,
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
            let projector_verification = M3ProjectorVerificationState::resolve(
                version.model.capabilities.vision,
                version.model.projector.as_ref(),
                version.projector_verified_sha256.as_deref(),
            );
            let vision_ready = version.model.capabilities.vision
                && projector_verification == M3ProjectorVerificationState::Verified
                && runtime_supports_image_transport(version.model.runtime);
            Ok(M3InstalledVersionView {
                version_key: version.version_key.clone(),
                revision: version.model.revision.clone(),
                sha256: version.model.sha256.clone(),
                size_bytes: version.model.size_bytes,
                artifact_path,
                installed_at_ms: version.installed_at_ms,
                active: version.version_key == stored.active_version_key,
                license: version.model.license.clone(),
                source_id: version.model.source_id.clone(),
                template: version.model.template.clone(),
                projector: version.model.projector.clone(),
                catalog_retrieved_at_ms: version.model.catalog_retrieved_at_ms,
                projector_verification,
                projector_verified_at_ms: version.projector_verified_at_ms,
                estimated_projector_memory_bytes: version
                    .model
                    .projector
                    .as_ref()
                    .map(estimated_projector_memory_bytes),
                vision_ready,
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

/// Finds an already-installed payload (under any asset or version) whose
/// digest and size match the requested download, re-hashing the on-disk
/// candidate before trusting it. A candidate whose bytes no longer match its
/// own manifest (e.g. bit rot) is silently skipped rather than reused, so a
/// corrupt local copy can never poison a new install; the caller falls back
/// to a real network download in that case.
fn find_reusable_payload(
    state: &M3HubState,
    models_root: &Path,
    sha256: &str,
    size_bytes: u64,
) -> M3HubResult<Option<PathBuf>> {
    for stored in &state.models {
        for version in &stored.versions {
            if version.model.sha256 != sha256 || version.model.size_bytes != size_bytes {
                continue;
            }
            let candidate = models_root.join(&version.artifact_relative_path);
            let Ok(true) = inspect_optional_regular(&candidate) else {
                continue;
            };
            match sha256_file(&candidate, size_bytes) {
                Ok(digest) if constant_time_eq(digest.as_bytes(), sha256.as_bytes()) => {
                    return Ok(Some(candidate));
                }
                _ => continue,
            }
        }
    }
    Ok(None)
}

/// Places verified bytes at `destination` by hard-linking `source` when
/// possible (sharing disk space with the existing copy) and falling back to
/// a full copy if linking is unavailable (e.g. across filesystems).
fn link_or_copy_owned(source: &Path, destination: &Path) -> M3HubResult<()> {
    if fs::hard_link(source, destination).is_ok() {
        return Ok(());
    }
    fs::copy(source, destination)
        .map(|_| ())
        .map_err(|error| io_at("reuse verified model payload", destination, error))
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

pub(crate) fn sha256_file(path: &Path, expected_size: u64) -> M3HubResult<String> {
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

fn validate_download_url(value: &str, allow_loopback_http: bool) -> M3HubResult<()> {
    let parsed = Url::parse(value).map_err(|error| invalid("downloadUrl", error.to_string()))?;
    if !allow_loopback_http {
        crate::egress::classify_public_download_url(
            &parsed,
            crate::egress::PublicDestinations::Only,
        )
        .map_err(|denial| invalid("downloadUrl", denial.to_string()))?;
    }
    validate_https_url(value, "downloadUrl", allow_loopback_http)
}

fn transport_error(error: reqwest::Error) -> M3HubError {
    match crate::egress::denial_from_error(&error) {
        Some(denial) => M3HubError::Transport(denial.to_string()),
        None => M3HubError::Transport(error.to_string()),
    }
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

#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
fn mlx_error(error: crate::mlx_runtime::MlxError) -> M3HubError {
    M3HubError::Runtime(error.to_string())
}

#[cfg(target_os = "macos")]
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

// =========================================================================
// Runtime Component Update Channels (ROADMAP.md Phase 8)
// =========================================================================
//
// This is a parallel, analogous system to the model manifest/blob/digest
// store above, but for the app's own runtime components — the `llama.cpp`
// server binary, the MLX runtime, tokenizers, converters, projector
// runtimes, and per-accelerator support packages (Metal/CUDA/ROCm/Vulkan)
// the app depends on to run models — rather than model weights. Models and
// components are deliberately kept as separate systems: they have different
// trust/licensing rules (component installs are not gated on end-user
// license acceptance the way model weights are) and different lifecycles
// (a component has a channel — stable, beta, or pinned — instead of a
// hardware-fit rating).
//
// The implementation intentionally mirrors the model system's shape
// wherever the concepts line up (content-addressed storage keyed by a
// digest-derived asset/version key, resumable chunked downloads, mandatory
// digest verification before anything is marked active, atomic state
// publication, activate-to-roll-back) and reuses every low-level primitive
// above it safely can: `M3DownloadTransport`/`M3Clock`, `M3ResumeState`,
// `prepare_resume_files`/`validate_download_chunk`, the atomic/private file
// helpers, digest verification, and the generation-based state file format
// (`parse_state_filename`/`prune_state_generations`). It does not reuse
// `M3RuntimeHub`'s own state or locks — components have an independent
// storage root and mutation lock so a component install/rollback can never
// block or be blocked by a model operation.
//
// There is no upstream binary registry/CDN for these artifacts, so — mirroring
// the pluggable `M3CatalogSource` pattern used for model catalogs —
// `M3ComponentSource` is a trait with a local, operator-editable
// implementation (`StaticM3ComponentSource`) rather than a hardcoded call to a
// registry this environment cannot confirm works. See
// `m3_production::component_registry_entries` for how production wiring loads
// that local registry, and the crate-level PR notes for why.
//
// What this project publishes for itself reaches that registry through
// `fetch_component_catalog`: the panel fetches a catalog document by URL and
// merges it in, so a published component is installable without the user
// downloading a JSON file and importing it by hand. The listing path stays
// local on purpose — entries are read from disk, so an unreachable catalog
// costs the newest versions rather than the whole panel.

pub const M3_COMPONENT_CATALOG_SCHEMA_VERSION: u32 = 1;
pub const M3_COMPONENT_STATE_VERSION: u32 = 1;
const COMPONENT_PAYLOAD_FILE: &str = "component.bin";
const COMPONENT_MANIFEST_FILE: &str = "component.json";
/// Bounded rollback history: the active version plus at most this many
/// additional recently-installed versions are kept on disk. This guarantees
/// at least one prior verified version is always available to roll back to
/// after any update, while never growing storage without bound.
const MAX_COMPONENT_VERSIONS_KEPT: usize = 3;
const MAX_INSTALLED_COMPONENTS: usize = 512;
const MAX_COMPONENT_SOURCES: usize = 64;
const MAX_COMPATIBILITY_NOTE_BYTES: usize = 4 * 1024;

/// Whether this build can do anything with a component of that kind.
///
/// A component feed is platform-agnostic — it lists every kind the project
/// publishes. Offering one this binary has no code for would be a download
/// button whose install step cannot exist: the MLX unpack-and-verify command is
/// compiled into the macOS build only, so a Windows or Linux user clicking
/// Install on `mlx_runtime` would fetch an archive and then hit a missing
/// command. Filtered at `list_registry`, the one place every listing path goes
/// through, rather than in each caller.
pub(crate) fn component_kind_runs_here(kind: M3ComponentKind) -> bool {
    match kind {
        M3ComponentKind::MlxRuntime => cfg!(target_os = "macos"),
        _ => true,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3ComponentKind {
    LlamaCppServer,
    MlxRuntime,
    Tokenizer,
    Converter,
    ProjectorRuntime,
    MetalSupport,
    CudaSupport,
    RocmSupport,
    VulkanSupport,
    /// A Studio sidecar tool: face swap, a detector, a segmenter. Not an
    /// inference runtime at all — it is a separate program speaking
    /// [`crate::studio_tools`]' HTTP contract, published by this project and
    /// fetched through the same digest-checked, versioned, rollback-capable
    /// path as every other component so a tool is never less verified than a
    /// runtime is.
    StudioTool,
}

/// Stable channel never auto-upgrades: it always tracks new verified
/// releases meant for general use. Beta tracks pre-release builds. Pinned
/// locks a component to one specific version indefinitely — `check_updates`
/// never reports an update for a pinned component, no matter what the
/// registry contains, until the operator explicitly installs a different
/// version (which re-pins to that new version).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum M3ComponentChannel {
    Stable,
    Beta,
    Pinned,
}

/// One known, downloadable version of a runtime component, as advertised by
/// an `M3ComponentSource`. This is the component analogue of
/// [`M3CatalogModel`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ComponentCatalogEntry {
    pub schema_version: u32,
    pub source_id: String,
    /// Stable identity for this component across versions/channels, e.g.
    /// `"llama-cpp-server-metal"` or `"tokenizer-bpe"`. Kept distinct from
    /// `kind` so more than one component can share a kind (for example two
    /// `llama_cpp_server` builds targeting different accelerators).
    pub component_id: String,
    pub kind: M3ComponentKind,
    pub display_name: String,
    pub accelerator: Option<crate::runtime_adapter::AcceleratorKind>,
    pub version: String,
    pub channel: M3ComponentChannel,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub published_at_ms: u64,
    /// Short human-readable note such as "requires driver >= 550" or "known
    /// issue on pre-Turing NVIDIA GPUs", surfaced verbatim in the UI.
    pub compatibility_note: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

impl M3ComponentCatalogEntry {
    fn asset_key(&self) -> String {
        sha256_hex(self.component_id.as_bytes())
    }

    fn version_key(&self) -> String {
        sha256_hex(format!("{}\n{}\n{}", self.version, self.sha256, self.download_url).as_bytes())
    }

    /// The one identity the registry dedupes, merges and updates on.
    ///
    /// Four fields, and the fourth is why this exists: `component_id`, `version`
    /// and `sha256` alone let two catalogs collide, so adopting one could
    /// *overwrite* an entry another publisher had registered for the same version
    /// while silently changing where its bytes come from. Including the URL makes
    /// the two entries two rows, which is the honest outcome — a version the app
    /// can fetch from two places is two things it knows, and the digest still
    /// decides whether either one installs.
    ///
    /// Composed from [`Self::version_key`] rather than restating its three fields,
    /// so the registry's identity and the *installed* version's identity cannot
    /// drift apart. `source_id` is deliberately not in it: the local registry
    /// restamps that field on adoption (see
    /// `m3_production::adopt_into_registry`), so it is the same for every row and
    /// could only ever make identity depend on when a row was written.
    pub(crate) fn registry_key(&self) -> String {
        format!("{}\n{}", self.component_id, self.version_key())
    }

    pub fn validate(&self) -> M3HubResult<()> {
        if self.schema_version != M3_COMPONENT_CATALOG_SCHEMA_VERSION {
            return Err(invalid("component.schemaVersion", "is unsupported"));
        }
        for (field, value) in [
            ("sourceId", self.source_id.as_str()),
            ("componentId", self.component_id.as_str()),
            ("displayName", self.display_name.as_str()),
            ("version", self.version.as_str()),
        ] {
            validate_identifier(value, &format!("component.{field}"))?;
        }
        validate_sha256(&self.sha256, "component.sha256")?;
        if self.size_bytes == 0 || self.size_bytes > MAX_DOWNLOAD_BYTES {
            return Err(invalid(
                "component.sizeBytes",
                format!("must be between 1 and {MAX_DOWNLOAD_BYTES}"),
            ));
        }
        validate_download_url(&self.download_url, cfg!(test))?;
        validate_timestamp(self.published_at_ms, "component.publishedAtMs")?;
        if let Some(note) = &self.compatibility_note {
            validate_text(
                note,
                "component.compatibilityNote",
                MAX_COMPATIBILITY_NOTE_BYTES,
            )?;
            if note.trim().is_empty() {
                return Err(invalid(
                    "component.compatibilityNote",
                    "must not be blank when present",
                ));
            }
        }
        if self.metadata.len() > 256 {
            return Err(invalid("component.metadata", "contains too many entries"));
        }
        for (key, value) in &self.metadata {
            validate_identifier(key, "component.metadata.key")?;
            validate_text(value, "component.metadata.value", 64 * 1024)?;
        }
        Ok(())
    }
}

/// A pluggable source of known runtime-component versions, mirroring
/// [`M3CatalogSource`]'s shape for model catalogs.
pub trait M3ComponentSource: Send + Sync {
    fn source_id(&self) -> &str;
    fn list<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<M3ComponentCatalogEntry>>;
}

/// A local, in-process registry of known component versions. This holds
/// whatever entries production wiring loaded from a local, operator-editable
/// file (see `m3_production::component_registry_entries`) instead of listing
/// versions from the network: a catalog fetch ([`fetch_component_catalog`])
/// writes into that file, so listing stays a disk read and being offline costs
/// nothing already known. Only the already-known `download_url` of a chosen
/// entry is ever fetched, through the same `M3DownloadTransport` used for
/// models.
pub struct StaticM3ComponentSource {
    source_id: String,
    entries: Vec<M3ComponentCatalogEntry>,
}

impl StaticM3ComponentSource {
    pub fn new(
        source_id: impl Into<String>,
        entries: Vec<M3ComponentCatalogEntry>,
    ) -> M3HubResult<Self> {
        let source_id = source_id.into();
        validate_identifier(&source_id, "componentSource.sourceId")?;
        if entries.len() > MAX_CATALOG_ENTRIES {
            return Err(invalid(
                "componentSource.entries",
                format!("at most {MAX_CATALOG_ENTRIES} entries are accepted"),
            ));
        }
        for entry in &entries {
            entry.validate()?;
            if entry.source_id != source_id {
                return Err(invalid(
                    "componentSource.sourceId",
                    "entry source differs from the configured source",
                ));
            }
        }
        Ok(Self { source_id, entries })
    }
}

impl M3ComponentSource for StaticM3ComponentSource {
    fn source_id(&self) -> &str {
        &self.source_id
    }

    fn list<'a>(
        &'a self,
        context: &'a M3OperationContext,
    ) -> M3HubFuture<'a, Vec<M3ComponentCatalogEntry>> {
        Box::pin(async move {
            context.preflight("component registry list")?;
            Ok(self.entries.clone())
        })
    }
}

/// A published catalog document: either a bare array of entries, or the
/// `{schemaVersion, entries}` wrapper the app writes for its own registry file.
///
/// Both are accepted for the same reason the file importer accepts both — the
/// second shape is what someone re-importing a backup of their own registry
/// has — and accepting them in one place keeps the fetch path and the file path
/// from drifting into disagreeing about what a catalog is.
#[derive(Deserialize)]
#[serde(untagged)]
enum M3ComponentCatalogDocument {
    Entries(Vec<M3ComponentCatalogEntry>),
    Envelope {
        #[serde(rename = "entries")]
        entries: Vec<M3ComponentCatalogEntry>,
    },
}

/// Whether installing a component of this kind proves the artifact came from this
/// project, rather than only that its bytes match a digest.
///
/// # Why a network catalog is limited by this
///
/// A catalog is discovery metadata. It names a URL, a size and a sha256, and a
/// SHA-256 the catalog itself supplied proves only that the bytes are the bytes
/// that catalog meant — whoever can replace the artifact can replace the digest
/// beside it in the same document. For [`M3ComponentKind::MlxRuntime`] that is not
/// the whole boundary: the installer re-derives every digest in the package
/// manifest and verifies an Ed25519 signature against a key compiled into this app
/// (`MLX_RELEASE_PUBLIC_KEY_HEX`), so a substituted archive fails to install no
/// matter what the catalog claimed. No other kind has that today.
///
/// So [`fetch_component_catalog`] refuses a fetched catalog that lists any other
/// kind. That is a rule in code and not a comment, and the match is exhaustive
/// with no wildcard arm on purpose: a component kind added later is a compile
/// error here, which is what stops "remotely installable" from becoming the silent
/// default for the next executable component this app learns to run.
///
/// Nothing stops an operator from registering those kinds by hand — **Import
/// catalog** and a directly edited registry file both still work, and both are
/// acts a person performed rather than a document a server served.
pub(crate) fn kind_verifies_publisher_signature(kind: M3ComponentKind) -> bool {
    match kind {
        M3ComponentKind::MlxRuntime => true,
        M3ComponentKind::LlamaCppServer
        | M3ComponentKind::Tokenizer
        | M3ComponentKind::Converter
        | M3ComponentKind::ProjectorRuntime
        | M3ComponentKind::MetalSupport
        | M3ComponentKind::CudaSupport
        | M3ComponentKind::RocmSupport
        | M3ComponentKind::VulkanSupport
        | M3ComponentKind::StudioTool => false,
    }
}

/// Fetches a component catalog over HTTP and returns the versions it lists.
///
/// Nothing is downloaded, installed, or persisted by this. Discovery and
/// authenticity are deliberately separate: what this returns is a list of URLs,
/// sizes and digests, and the checks that establish trust — the declared size, the
/// whole-artifact SHA-256, and the pinned publisher key — all still happen at
/// install time against the artifact itself. A hostile catalog can therefore cost
/// a failed install, and the one thing it must not be able to do is *become* the
/// authenticity boundary, which is what [`kind_verifies_publisher_signature`]
/// refuses below.
pub async fn fetch_component_catalog(
    endpoint: &str,
    context: &M3OperationContext,
) -> M3HubResult<Vec<M3ComponentCatalogEntry>> {
    context.preflight("component catalog fetch")?;
    // Loopback HTTP is allowed for the same reason `HttpM3CatalogSource` allows
    // it — a catalog served by something on this machine — but only when the
    // *configured* endpoint is itself loopback. Deriving the permission from the
    // endpoint and then handing that one decision to the client is what stops a
    // public catalog from redirecting the fetch into a local service: with a public
    // endpoint, neither a hop nor a resolved address may be loopback at all.
    let destinations = if crate::egress::is_loopback_target(
        &Url::parse(endpoint)
            .map_err(|error| invalid("componentCatalog.url", error.to_string()))?,
    ) {
        crate::egress::PublicDestinations::LoopbackAllowed
    } else {
        crate::egress::PublicDestinations::Only
    };
    validate_download_url(
        endpoint,
        destinations == crate::egress::PublicDestinations::LoopbackAllowed,
    )?;
    let client = crate::egress::public_download_client(destinations, COMPONENT_EGRESS_GUARD)
        .build()
        .map_err(|error| M3HubError::Transport(error.to_string()))?;
    let response = run_bounded(context, "component catalog fetch", async {
        crate::egress::send(client.get(endpoint))
            .await
            .map_err(transport_error)
    })
    .await?;
    refuse_unfollowable_redirect(&response, "componentCatalog.url")?;
    if !response.status().is_success() {
        return Err(M3HubError::Transport(format!(
            "component catalog returned HTTP {}",
            response.status()
        )));
    }
    let bytes = read_response_bounded(response, MAX_CATALOG_BODY_BYTES, context).await?;
    let entries = parse_component_catalog(&bytes)?;
    for entry in &entries {
        if !kind_verifies_publisher_signature(entry.kind) {
            return Err(invalid(
                "componentCatalog.kind",
                format!(
                    "a fetched catalog may only list components whose install verifies a pinned \
                     publisher key; {:?} does not, so register it with Import catalog instead",
                    entry.kind
                ),
            ));
        }
    }
    Ok(entries)
}

fn parse_component_catalog(bytes: &[u8]) -> M3HubResult<Vec<M3ComponentCatalogEntry>> {
    let entries = match serde_json::from_slice::<M3ComponentCatalogDocument>(bytes)? {
        M3ComponentCatalogDocument::Entries(entries) => entries,
        M3ComponentCatalogDocument::Envelope { entries } => entries,
    };
    if entries.len() > MAX_CATALOG_ENTRIES {
        return Err(invalid(
            "componentCatalog.entries",
            format!("at most {MAX_CATALOG_ENTRIES} entries are accepted"),
        ));
    }
    // Every entry is validated here rather than at install time, so a catalog
    // that is malformed anywhere is refused whole instead of half-adopted.
    for entry in &entries {
        entry.validate()?;
    }
    Ok(entries)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3StoredComponentVersion {
    version_key: String,
    entry: M3ComponentCatalogEntry,
    artifact_relative_path: String,
    installed_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3StoredComponent {
    component_id: String,
    asset_key: String,
    active_version_key: String,
    versions: Vec<M3StoredComponentVersion>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct M3ComponentHubState {
    state_version: u32,
    generation: u64,
    updated_at_ms: u64,
    components: Vec<M3StoredComponent>,
}

impl Default for M3ComponentHubState {
    fn default() -> Self {
        Self {
            state_version: M3_COMPONENT_STATE_VERSION,
            generation: 0,
            updated_at_ms: 0,
            components: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3InstallComponentRequest {
    pub entry: M3ComponentCatalogEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ActivateComponentVersionRequest {
    pub component_id: String,
    pub version_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3InstalledComponentVersionView {
    pub version_key: String,
    pub version: String,
    pub channel: M3ComponentChannel,
    pub sha256: String,
    pub size_bytes: u64,
    pub source_url: String,
    pub artifact_path: PathBuf,
    pub installed_at_ms: u64,
    pub published_at_ms: u64,
    pub active: bool,
    pub compatibility_note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3InstalledComponentView {
    pub component_id: String,
    pub kind: M3ComponentKind,
    pub display_name: String,
    pub accelerator: Option<crate::runtime_adapter::AcceleratorKind>,
    /// Channel of the currently active version. Determines whether
    /// `check_updates` may ever report an update for this component.
    pub channel: M3ComponentChannel,
    pub active_version_key: String,
    pub versions: Vec<M3InstalledComponentVersionView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct M3ComponentUpdateCheck {
    pub component_id: String,
    pub channel: M3ComponentChannel,
    pub installed_version: String,
    pub installed_published_at_ms: u64,
    pub latest_available: Option<M3ComponentCatalogEntry>,
    pub update_available: bool,
}

pub struct M3ComponentHubDependencies {
    pub clock: Arc<dyn M3Clock>,
    pub download: Arc<dyn M3DownloadTransport>,
    pub sources: Vec<Arc<dyn M3ComponentSource>>,
}

/// Versioned-component manager: the parallel, component-focused counterpart
/// to [`M3RuntimeHub`]'s model manifest/blob/digest store. See the module
/// section header above for how it relates to that system.
pub struct M3ComponentHub {
    config: M3HubConfig,
    root: PathBuf,
    blobs_root: PathBuf,
    downloads_root: PathBuf,
    state_root: PathBuf,
    clock: Arc<dyn M3Clock>,
    download: Arc<dyn M3DownloadTransport>,
    sources: RwLock<Vec<Arc<dyn M3ComponentSource>>>,
    state_lock: Mutex<()>,
    mutation_lock: tokio::sync::Mutex<()>,
}

impl M3ComponentHub {
    pub fn new(
        root: impl AsRef<Path>,
        config: M3HubConfig,
        dependencies: M3ComponentHubDependencies,
    ) -> M3HubResult<Self> {
        config.validate()?;
        validate_component_sources(&dependencies.sources)?;
        let root = root.as_ref().to_path_buf();
        if !root.is_absolute() {
            return Err(invalid("root", "must be an absolute app-private path"));
        }
        ensure_private_directory(&root)?;
        let blobs_root = root.join("blobs");
        let downloads_root = root.join("downloads");
        let state_root = root.join("state");
        for directory in [&blobs_root, &downloads_root, &state_root] {
            ensure_private_directory(directory)?;
        }
        // Fail fast on construction if the durable store is corrupt,
        // mirroring `M3RuntimeHub::new`'s eager validation.
        load_component_state(&state_root, &blobs_root)?;
        Ok(Self {
            config,
            root,
            blobs_root,
            downloads_root,
            state_root,
            clock: dependencies.clock,
            download: dependencies.download,
            sources: RwLock::new(dependencies.sources),
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

    pub fn replace_sources(&self, sources: Vec<Arc<dyn M3ComponentSource>>) -> M3HubResult<()> {
        validate_component_sources(&sources)?;
        *write_lock(&self.sources)? = sources;
        Ok(())
    }

    /// Reuses the model system's `M3StorageStatus` shape: `used_bytes` and
    /// `available_for_models_bytes` describe this hub's own component blob
    /// storage tree, not the model store.
    pub fn storage_status(&self) -> M3HubResult<M3StorageStatus> {
        let blob_bytes = directory_size(&self.blobs_root)?;
        let pending_download_bytes = directory_size(&self.downloads_root)?;
        let used_bytes = blob_bytes
            .checked_add(pending_download_bytes)
            .ok_or_else(|| {
                M3HubError::State("managed component storage byte count overflow".to_string())
            })?;
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

    pub async fn list_registry(
        &self,
        context: &M3OperationContext,
    ) -> M3HubResult<Vec<M3ComponentCatalogEntry>> {
        context.preflight("component registry list")?;
        let sources = read_lock(&self.sources)?.clone();
        let mut entries = Vec::new();
        let mut dedupe = BTreeSet::new();
        for source in &sources {
            let listed =
                run_bounded(context, "component source list", source.list(context)).await?;
            if listed.len() > MAX_CATALOG_ENTRIES {
                return Err(invalid(
                    "component.entries",
                    "source returned too many entries",
                ));
            }
            for entry in listed {
                entry.validate()?;
                if !component_kind_runs_here(entry.kind) {
                    continue;
                }
                if entry.source_id != source.source_id() {
                    return Err(invalid(
                        "component.sourceId",
                        "source returned an entry for another source",
                    ));
                }
                // The registry's one identity, so listing dedupes on exactly what
                // merging and update detection key on. It used to be
                // `component_id`/`version`/`sha256` spelled out here, which
                // collapsed two entries that differ only in `download_url` into
                // whichever one a source happened to list first.
                if dedupe.insert(entry.registry_key()) {
                    entries.push(entry);
                }
            }
        }
        entries.sort_by(|left, right| {
            left.component_id
                .cmp(&right.component_id)
                .then_with(|| right.published_at_ms.cmp(&left.published_at_ms))
        });
        Ok(entries)
    }

    pub fn list_installed(&self) -> M3HubResult<Vec<M3InstalledComponentView>> {
        let _guard = lock(&self.state_lock)?;
        let state = load_component_state(&self.state_root, &self.blobs_root)?;
        component_state_to_views(&state, &self.blobs_root)
    }

    pub async fn check_updates(
        &self,
        context: &M3OperationContext,
    ) -> M3HubResult<Vec<M3ComponentUpdateCheck>> {
        context.preflight("component update check")?;
        let installed = self.list_installed()?;
        let registry = self.list_registry(context).await?;
        let mut checks = Vec::with_capacity(installed.len());
        for component in &installed {
            let active = component
                .versions
                .iter()
                .find(|version| version.active)
                .ok_or_else(|| {
                    M3HubError::State("active component version is missing".to_string())
                })?;
            let latest = if component.channel == M3ComponentChannel::Pinned {
                None
            } else {
                registry
                    .iter()
                    .filter(|entry| {
                        entry.component_id == component.component_id
                            && entry.channel == component.channel
                    })
                    .max_by_key(|entry| entry.published_at_ms)
                    .cloned()
            };
            let update_available = latest
                .as_ref()
                .is_some_and(|entry| entry.version_key() != component.active_version_key);
            checks.push(M3ComponentUpdateCheck {
                component_id: component.component_id.clone(),
                channel: component.channel,
                installed_version: active.version.clone(),
                installed_published_at_ms: active.published_at_ms,
                latest_available: latest,
                update_available,
            });
        }
        Ok(checks)
    }

    /// Downloads (resumably, digest-verified), installs, and activates a
    /// component version. Mirrors `M3RuntimeHub::download_model`'s shape:
    /// probe, chunked range reads into a partial file, whole-file digest
    /// verification, then an atomic stage-then-rename publish. Never trusts
    /// an unverified download — the digest check happens before the payload
    /// is ever moved into the content-addressed blob tree.
    pub async fn install_component(
        &self,
        request: &M3InstallComponentRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledComponentView> {
        context.preflight("install component")?;
        request.entry.validate()?;
        let _mutation = self.mutation_lock.lock().await;
        let component_id = request.entry.component_id.clone();
        let asset_key = request.entry.asset_key();
        let version_key = request.entry.version_key();

        {
            let _guard = lock(&self.state_lock)?;
            let state = load_component_state(&self.state_root, &self.blobs_root)?;
            if let Some(existing) = state.components.iter().find(|component| {
                component.component_id == component_id
                    && component.active_version_key == version_key
                    && component.versions.iter().any(|version| {
                        version.version_key == version_key
                            && version.entry.sha256 == request.entry.sha256
                    })
            }) {
                verify_stored_component(existing, &self.blobs_root)?;
                return stored_component_view(existing, &self.blobs_root);
            }
        }

        let probe = run_bounded(
            context,
            "probe component download",
            self.download.probe(&request.entry.download_url, context),
        )
        .await?;
        if probe.total_bytes != request.entry.size_bytes {
            return Err(invalid(
                "component.download.contentLength",
                format!(
                    "registry declares {} bytes but server declares {}",
                    request.entry.size_bytes, probe.total_bytes
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
            schema_version: M3_COMPONENT_CATALOG_SCHEMA_VERSION,
            asset_key: asset_key.clone(),
            version_key: version_key.clone(),
            url: request.entry.download_url.clone(),
            expected_sha256: request.entry.sha256.clone(),
            total_bytes: request.entry.size_bytes,
            etag: probe.etag.clone(),
        };
        prepare_resume_files(&partial_path, &resume_path, &expected_resume, &probe)?;
        atomic_write_private(&resume_path, &canonical_json(&expected_resume)?)?;
        let mut offset = regular_file_len_or_zero(&partial_path)?;
        if offset > 0 && !probe.accepts_ranges {
            remove_owned_file(&partial_path)?;
            offset = 0;
        }
        let remaining = request.entry.size_bytes.saturating_sub(offset);
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
            .map_err(|source| io_at("open partial component", &partial_path, source))?;
        while offset < request.entry.size_bytes {
            context.preflight("install component")?;
            let requested = usize::try_from(
                (request.entry.size_bytes - offset).min(self.config.download_chunk_bytes as u64),
            )
            .map_err(|_| invalid("component.download.range", "size conversion overflow"))?;
            let chunk = run_bounded(
                context,
                "download component range",
                self.download.read_range(
                    &request.entry.download_url,
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
                .map_err(|source| io_at("append partial component", &partial_path, source))?;
            offset = offset
                .checked_add(chunk.bytes.len() as u64)
                .ok_or_else(|| invalid("component.download.offset", "overflow"))?;
        }
        drop(partial);
        let actual_digest = sha256_file(&partial_path, request.entry.size_bytes)?;
        if !constant_time_eq(actual_digest.as_bytes(), request.entry.sha256.as_bytes()) {
            remove_owned_file(&partial_path)?;
            remove_owned_file(&resume_path)?;
            return Err(M3HubError::Integrity {
                expected: request.entry.sha256.clone(),
                actual: actual_digest,
            });
        }

        let asset_root = self.blobs_root.join(&asset_key);
        ensure_private_directory(&asset_root)?;
        let final_root = asset_root.join(&version_key);
        if final_root.exists() {
            verify_component_directory(&final_root, &request.entry)?;
            remove_owned_file(&partial_path)?;
        } else {
            let staging = asset_root.join(format!(".staging-{}", Uuid::new_v4()));
            ensure_private_directory(&staging)?;
            let staging_payload = staging.join(COMPONENT_PAYLOAD_FILE);
            fs::rename(&partial_path, &staging_payload)
                .map_err(|source| io_at("stage verified component", &staging_payload, source))?;
            harden_file(&staging_payload)?;
            let manifest_path = staging.join(COMPONENT_MANIFEST_FILE);
            atomic_write_private(&manifest_path, &canonical_json(&request.entry)?)?;
            sync_directory(&staging)?;
            fs::rename(&staging, &final_root)
                .map_err(|source| io_at("publish verified component", &final_root, source))?;
            sync_directory(&asset_root)?;
        }
        remove_owned_file(&resume_path)?;
        let artifact_relative_path = relative_component_payload(&asset_key, &version_key);
        let installed_at_ms = self.clock.now_ms()?;

        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_component_state(&self.state_root, &self.blobs_root)?
        };
        let index = state
            .components
            .iter()
            .position(|component| component.component_id == component_id);
        let stored_version = M3StoredComponentVersion {
            version_key: version_key.clone(),
            entry: request.entry.clone(),
            artifact_relative_path,
            installed_at_ms,
        };
        match index {
            Some(index) => {
                let stored = &mut state.components[index];
                if stored.asset_key != asset_key {
                    return Err(M3HubError::State(
                        "component id maps to an unexpected storage key".to_string(),
                    ));
                }
                if let Some(version) = stored
                    .versions
                    .iter_mut()
                    .find(|version| version.version_key == version_key)
                {
                    *version = stored_version;
                } else {
                    stored.versions.push(stored_version);
                }
                stored.active_version_key = version_key.clone();
            }
            None => {
                if state.components.len() >= MAX_INSTALLED_COMPONENTS {
                    return Err(M3HubError::Conflict(format!(
                        "installed component count reached {MAX_INSTALLED_COMPONENTS}"
                    )));
                }
                state.components.push(M3StoredComponent {
                    component_id: component_id.clone(),
                    asset_key,
                    active_version_key: version_key,
                    versions: vec![stored_version],
                });
                state
                    .components
                    .sort_by(|left, right| left.component_id.cmp(&right.component_id));
            }
        }

        let component_index = state
            .components
            .iter()
            .position(|component| component.component_id == component_id)
            .ok_or_else(|| M3HubError::State("installed component vanished".to_string()))?;
        let asset_key_for_prune = state.components[component_index].asset_key.clone();
        let prune_root = self.blobs_root.join(&asset_key_for_prune);
        let pruned = prune_excess_component_versions(
            &mut state.components[component_index],
            &self.blobs_root,
        )?;
        let candidate =
            match stored_component_view(&state.components[component_index], &self.blobs_root) {
                Ok(view) => view,
                Err(error) => {
                    restore_isolated_versions(&pruned, &prune_root);
                    return Err(error);
                }
            };
        let saved = {
            let _guard = lock(&self.state_lock)?;
            save_next_component_state(&self.state_root, &mut state, self.clock.now_ms()?)
        };
        if let Err(error) = saved {
            restore_isolated_versions(&pruned, &prune_root);
            return Err(error);
        }
        for (isolated_path, _) in &pruned {
            remove_owned_directory(isolated_path)?;
        }
        sync_directory(&prune_root)?;
        Ok(candidate)
    }

    /// Atomically activates an already-installed, digest-verified version —
    /// this is also how rollback works: the UI calls this with an older
    /// installed version's key. Mirrors
    /// `M3RuntimeHub::activate_model_version`'s verify-then-swap shape.
    pub async fn activate_component_version(
        &self,
        request: &M3ActivateComponentVersionRequest,
        context: &M3OperationContext,
    ) -> M3HubResult<M3InstalledComponentView> {
        validate_identifier(&request.component_id, "componentId")?;
        validate_sha256(&request.version_key, "versionKey")?;
        context.preflight("activate component version")?;
        let _mutation = self.mutation_lock.lock().await;
        let mut state = {
            let _guard = lock(&self.state_lock)?;
            load_component_state(&self.state_root, &self.blobs_root)?
        };
        let index = state
            .components
            .iter()
            .position(|component| component.component_id == request.component_id)
            .ok_or_else(|| M3HubError::NotFound(request.component_id.clone()))?;
        let stored = state.components[index].clone();
        let target = stored
            .versions
            .iter()
            .find(|version| version.version_key == request.version_key)
            .ok_or_else(|| M3HubError::NotFound(request.version_key.clone()))?;
        verify_component_directory(
            &self
                .blobs_root
                .join(&stored.asset_key)
                .join(&target.version_key),
            &target.entry,
        )?;
        if stored.active_version_key == request.version_key {
            return stored_component_view(&stored, &self.blobs_root);
        }
        state.components[index].active_version_key = request.version_key.clone();
        let candidate = stored_component_view(&state.components[index], &self.blobs_root)?;
        {
            let _guard = lock(&self.state_lock)?;
            save_next_component_state(&self.state_root, &mut state, self.clock.now_ms()?)?;
        }
        Ok(candidate)
    }
}

fn prune_excess_component_versions(
    stored: &mut M3StoredComponent,
    blobs_root: &Path,
) -> M3HubResult<Vec<(PathBuf, PathBuf)>> {
    if stored.versions.len() <= MAX_COMPONENT_VERSIONS_KEPT {
        return Ok(Vec::new());
    }
    let asset_root = blobs_root.join(&stored.asset_key);
    let mut ordered: Vec<(u64, String)> = stored
        .versions
        .iter()
        .map(|version| (version.installed_at_ms, version.version_key.clone()))
        .collect();
    ordered.sort_by(|left, right| right.0.cmp(&left.0));
    let mut kept: BTreeSet<String> = BTreeSet::new();
    kept.insert(stored.active_version_key.clone());
    for (_, version_key) in &ordered {
        if kept.len() >= MAX_COMPONENT_VERSIONS_KEPT {
            break;
        }
        kept.insert(version_key.clone());
    }
    let mut isolated = Vec::new();
    for version in stored
        .versions
        .iter()
        .filter(|version| !kept.contains(&version.version_key))
    {
        let source = asset_root.join(&version.version_key);
        let destination = asset_root.join(format!(".trash-auto-{}", version.version_key));
        if let Err(source_error) = fs::rename(&source, &destination) {
            restore_isolated_versions(&isolated, &asset_root);
            return Err(io_at(
                "isolate excess component version",
                &source,
                source_error,
            ));
        }
        isolated.push((destination, source));
    }
    stored
        .versions
        .retain(|version| kept.contains(&version.version_key));
    Ok(isolated)
}

fn load_component_state(state_root: &Path, blobs_root: &Path) -> M3HubResult<M3ComponentHubState> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(state_root)
        .map_err(|source| io_at("list M3 component state", state_root, source))?
    {
        let entry =
            entry.map_err(|source| io_at("read M3 component state entry", state_root, source))?;
        let Some((generation, digest_prefix)) = parse_state_filename(&entry.file_name()) else {
            continue;
        };
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_at("inspect M3 component state", &path, source))?;
        if !metadata.file_type().is_file() {
            return Err(M3HubError::State(
                "component state generation is not a regular file".to_string(),
            ));
        }
        candidates.push((generation, digest_prefix, path, metadata.len()));
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
    let Some((filename_generation, digest_prefix, path, size)) = candidates.first() else {
        return Ok(M3ComponentHubState::default());
    };
    if *size > MAX_STATE_BYTES as u64 {
        return Err(M3HubError::State(
            "component state generation exceeds the byte limit".to_string(),
        ));
    }
    let bytes = fs::read(path).map_err(|source| io_at("read M3 component state", path, source))?;
    let actual_digest = sha256_hex(&bytes);
    if !actual_digest.starts_with(digest_prefix) {
        return Err(M3HubError::State(
            "component state filename digest does not match its bytes".to_string(),
        ));
    }
    let state: M3ComponentHubState = serde_json::from_slice(&bytes)?;
    if state.generation != *filename_generation {
        return Err(M3HubError::State(
            "component state filename generation does not match its payload".to_string(),
        ));
    }
    validate_component_hub_state(&state, blobs_root)?;
    Ok(state)
}

fn save_next_component_state(
    state_root: &Path,
    state: &mut M3ComponentHubState,
    now_ms: u64,
) -> M3HubResult<()> {
    validate_timestamp(now_ms, "componentState.updatedAtMs")?;
    state.generation = state
        .generation
        .checked_add(1)
        .ok_or_else(|| M3HubError::State("component state generation overflow".to_string()))?;
    state.updated_at_ms = now_ms;
    validate_component_hub_state_structure(state)?;
    let bytes = canonical_json(state)?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(M3HubError::State(
            "component state exceeds the byte limit".to_string(),
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
        .map_err(|source| io_at("create M3 component state", &path, source))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|source| io_at("write M3 component state", &path, source))?;
    sync_directory(state_root)?;
    prune_state_generations(state_root, state.generation)?;
    Ok(())
}

fn validate_component_hub_state(state: &M3ComponentHubState, blobs_root: &Path) -> M3HubResult<()> {
    validate_component_hub_state_structure(state)?;
    for stored in &state.components {
        for version in &stored.versions {
            let artifact = blobs_root.join(&version.artifact_relative_path);
            ensure_descendant(blobs_root, &artifact)?;
            let metadata = fs::symlink_metadata(&artifact)
                .map_err(|source| io_at("inspect installed component", &artifact, source))?;
            if !metadata.file_type().is_file() || metadata.len() != version.entry.size_bytes {
                return Err(M3HubError::State(format!(
                    "installed component {} has missing or invalid payload metadata",
                    stored.component_id
                )));
            }
        }
    }
    Ok(())
}

fn validate_component_hub_state_structure(state: &M3ComponentHubState) -> M3HubResult<()> {
    if state.state_version != M3_COMPONENT_STATE_VERSION {
        return Err(M3HubError::State(
            "unsupported M3 component state version".to_string(),
        ));
    }
    if state.generation == 0 {
        if state.updated_at_ms != 0 || !state.components.is_empty() {
            return Err(M3HubError::State(
                "generation zero is reserved for empty in-memory component state".to_string(),
            ));
        }
    } else {
        validate_timestamp(state.updated_at_ms, "componentState.updatedAtMs")?;
    }
    if state.components.len() > MAX_INSTALLED_COMPONENTS {
        return Err(M3HubError::State(
            "installed component count exceeds the limit".to_string(),
        ));
    }
    let mut component_ids = BTreeSet::new();
    let mut asset_keys = BTreeSet::new();
    for stored in &state.components {
        validate_identifier(&stored.component_id, "componentState.componentId")?;
        validate_sha256(&stored.asset_key, "componentState.assetKey")?;
        if stored.asset_key != sha256_hex(stored.component_id.as_bytes()) {
            return Err(M3HubError::State(
                "stored component asset key does not derive from its id".to_string(),
            ));
        }
        if !component_ids.insert(&stored.component_id) || !asset_keys.insert(&stored.asset_key) {
            return Err(M3HubError::State(
                "installed component ids and keys must be unique".to_string(),
            ));
        }
        if stored.versions.is_empty() || stored.versions.len() > MAX_COMPONENT_VERSIONS_KEPT {
            return Err(M3HubError::State(
                "installed component version count is invalid".to_string(),
            ));
        }
        let mut version_keys = BTreeSet::new();
        for version in &stored.versions {
            version.entry.validate()?;
            if version.entry.component_id != stored.component_id
                || version.version_key != version.entry.version_key()
            {
                return Err(M3HubError::State(
                    "installed component version identity differs from its registry record"
                        .to_string(),
                ));
            }
            validate_sha256(&version.version_key, "componentState.versionKey")?;
            validate_timestamp(version.installed_at_ms, "componentState.installedAtMs")?;
            if version.artifact_relative_path
                != relative_component_payload(&stored.asset_key, &version.version_key)
            {
                return Err(M3HubError::State(
                    "installed component artifact path is not canonical".to_string(),
                ));
            }
            validate_relative_path(&version.artifact_relative_path)?;
            if !version_keys.insert(&version.version_key) {
                return Err(M3HubError::State(
                    "installed component version keys must be unique".to_string(),
                ));
            }
        }
        if !version_keys.contains(&stored.active_version_key) {
            return Err(M3HubError::State(
                "active component version does not exist".to_string(),
            ));
        }
    }
    Ok(())
}

fn component_state_to_views(
    state: &M3ComponentHubState,
    blobs_root: &Path,
) -> M3HubResult<Vec<M3InstalledComponentView>> {
    let mut output = state
        .components
        .iter()
        .map(|component| stored_component_view(component, blobs_root))
        .collect::<M3HubResult<Vec<_>>>()?;
    output.sort_by(|left, right| left.component_id.cmp(&right.component_id));
    Ok(output)
}

fn stored_component_view(
    stored: &M3StoredComponent,
    blobs_root: &Path,
) -> M3HubResult<M3InstalledComponentView> {
    let active = stored
        .versions
        .iter()
        .find(|version| version.version_key == stored.active_version_key)
        .ok_or_else(|| M3HubError::State("active component version is missing".to_string()))?;
    let mut versions = stored
        .versions
        .iter()
        .map(|version| {
            let artifact_path = blobs_root.join(&version.artifact_relative_path);
            ensure_descendant(blobs_root, &artifact_path)?;
            Ok(M3InstalledComponentVersionView {
                version_key: version.version_key.clone(),
                version: version.entry.version.clone(),
                channel: version.entry.channel,
                sha256: version.entry.sha256.clone(),
                size_bytes: version.entry.size_bytes,
                source_url: version.entry.download_url.clone(),
                artifact_path,
                installed_at_ms: version.installed_at_ms,
                published_at_ms: version.entry.published_at_ms,
                active: version.version_key == stored.active_version_key,
                compatibility_note: version.entry.compatibility_note.clone(),
            })
        })
        .collect::<M3HubResult<Vec<_>>>()?;
    versions.sort_by(|left, right| right.installed_at_ms.cmp(&left.installed_at_ms));
    Ok(M3InstalledComponentView {
        component_id: stored.component_id.clone(),
        kind: active.entry.kind,
        display_name: active.entry.display_name.clone(),
        accelerator: active.entry.accelerator,
        channel: active.entry.channel,
        active_version_key: stored.active_version_key.clone(),
        versions,
    })
}

fn verify_stored_component(stored: &M3StoredComponent, blobs_root: &Path) -> M3HubResult<()> {
    let active = stored
        .versions
        .iter()
        .find(|version| version.version_key == stored.active_version_key)
        .ok_or_else(|| M3HubError::State("active component version is missing".to_string()))?;
    verify_component_directory(
        &blobs_root.join(&stored.asset_key).join(&active.version_key),
        &active.entry,
    )
}

fn verify_component_directory(
    directory: &Path,
    expected: &M3ComponentCatalogEntry,
) -> M3HubResult<()> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|source| io_at("inspect component version", directory, source))?;
    if !metadata.file_type().is_dir() {
        return Err(M3HubError::State(
            "component version is not a real directory".to_string(),
        ));
    }
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(directory)
        .map_err(|source| io_at("list component version", directory, source))?
    {
        let entry =
            entry.map_err(|source| io_at("read component version entry", directory, source))?;
        let name = entry
            .file_name()
            .to_str()
            .ok_or_else(|| M3HubError::State("component entry name is not UTF-8".to_string()))?
            .to_string();
        let entry_metadata = entry
            .metadata()
            .map_err(|source| io_at("inspect component version entry", &entry.path(), source))?;
        if !entry_metadata.is_file()
            || !matches!(
                name.as_str(),
                COMPONENT_PAYLOAD_FILE | COMPONENT_MANIFEST_FILE
            )
        {
            return Err(M3HubError::State(
                "component version contains an unexpected entry".to_string(),
            ));
        }
        names.insert(name);
    }
    if names
        != BTreeSet::from([
            COMPONENT_MANIFEST_FILE.to_string(),
            COMPONENT_PAYLOAD_FILE.to_string(),
        ])
    {
        return Err(M3HubError::State(
            "component version is missing required files".to_string(),
        ));
    }
    let manifest_path = directory.join(COMPONENT_MANIFEST_FILE);
    let manifest: M3ComponentCatalogEntry =
        serde_json::from_slice(&read_regular_bounded(&manifest_path, MAX_STATE_BYTES)?)?;
    if &manifest != expected {
        return Err(M3HubError::State(
            "component manifest differs from authenticated hub state".to_string(),
        ));
    }
    let payload = directory.join(COMPONENT_PAYLOAD_FILE);
    let digest = sha256_file(&payload, expected.size_bytes)?;
    if !constant_time_eq(digest.as_bytes(), expected.sha256.as_bytes()) {
        return Err(M3HubError::Integrity {
            expected: expected.sha256.clone(),
            actual: digest,
        });
    }
    Ok(())
}

fn relative_component_payload(asset_key: &str, version_key: &str) -> String {
    format!("{asset_key}/{version_key}/{COMPONENT_PAYLOAD_FILE}")
}

fn validate_component_sources(sources: &[Arc<dyn M3ComponentSource>]) -> M3HubResult<()> {
    if sources.len() > MAX_COMPONENT_SOURCES {
        return Err(invalid(
            "componentSources",
            format!("at most {MAX_COMPONENT_SOURCES} sources are accepted"),
        ));
    }
    let mut source_ids = BTreeSet::new();
    for source in sources {
        validate_identifier(source.source_id(), "componentSource.sourceId")?;
        if !source_ids.insert(source.source_id().to_string()) {
            return Err(invalid("componentSources", "source ids must be unique"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compatibility_hub::CanonicalToolDefinition;

    struct RegistryTestHardware;

    impl M3HardwareProbe for RegistryTestHardware {
        fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
            Ok(HardwareSnapshot {
                captured_at_ms: 1,
                total_ram_bytes: 8 * 1024 * 1024 * 1024,
                available_ram_bytes: 6 * 1024 * 1024 * 1024,
                logical_cpu_count: 4,
                platform: crate::runtime_adapter::PlatformCapabilities::current(Vec::new()),
            })
        }
    }

    #[test]
    fn stale_cancel_binding_cannot_target_a_reused_request_id() {
        let root = std::env::temp_dir().join(format!(
            "m3-cancel-aba-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("create ABA test root");
        let hub = M3RuntimeHub::new(
            &root,
            M3HubConfig::default(),
            M3RuntimeHubDependencies {
                clock: Arc::new(SystemM3Clock),
                hardware: Arc::new(RegistryTestHardware),
                download: Arc::new(
                    ReqwestM3DownloadTransport::new().expect("ABA test download transport"),
                ),
                catalogs: Vec::new(),
                runtimes: Vec::new(),
                runtime_reconciler: None,
                lan_factory: None,
            },
        )
        .expect("ABA test hub");
        let first = hub
            .register_in_flight_inference(
                "reused-request",
                "managed-runtime",
                "local-model",
                ApiScope::ChatCompletions,
                &M3RequestPrincipal::PairedToken("token-a".to_string()),
            )
            .expect("register first request generation");
        let stale = hub
            .in_flight_inference_binding("reused-request")
            .expect("capture first binding");
        drop(first);
        let _second = hub
            .register_in_flight_inference(
                "reused-request",
                "managed-runtime",
                "local-model",
                ApiScope::ChatCompletions,
                &M3RequestPrincipal::PairedToken("token-a".to_string()),
            )
            .expect("reuse requestId after first dispatch finished");

        assert!(matches!(
            hub.begin_in_flight_cancellation("reused-request", stale.registration_id),
            Err(M3HubError::NotFound(_))
        ));
        assert!(hub.in_flight_inference_binding("reused-request").is_ok());
        drop(_second);
        drop(hub);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn api_caller_debug_and_serialization_redact_plaintext_bearers() {
        let caller = M3ApiCaller::External {
            bearer_token: "lmk-lan-super-secret".to_string(),
            remote_address: "127.0.0.1".to_string(),
        };
        let debug = format!("{caller:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("super-secret"));
        let serialized = serde_json::to_string(&caller).expect("serialize caller");
        assert!(!serialized.contains("super-secret"));
    }

    // ------------------------------------------------------------------
    // Phase 8 item 10: tool-call and structured-output parser hardening.
    //
    // This module previously had no test coverage at all, despite owning
    // `CanonicalCollector` (which turns a `CanonicalStreamEvent` sequence
    // into the final `CanonicalInferenceResponse` for both `.complete()`
    // implementations) and `MlxCanonicalSink` (which translates the MLX
    // runtime's own event protocol into canonical events). These fixtures
    // exercise both directly against adversarial/malformed input, the same
    // way the m3_production.rs OpenAI-compatible-engine fixtures do.
    // ------------------------------------------------------------------

    fn request_with_tools(tools: &[&str]) -> CanonicalInferenceRequest {
        CanonicalInferenceRequest {
            schema_version: crate::compatibility_hub::COMPATIBILITY_SCHEMA_VERSION,
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            request_id: "request-mlx".to_string(),
            model: "local-model".to_string(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "hi".to_string(),
                }],
            }],
            tools: tools
                .iter()
                .map(|name| CanonicalToolDefinition {
                    name: name.to_string(),
                    description: "test tool".to_string(),
                    input_schema: json!({"type":"object","properties":{}}),
                    strict: false,
                })
                .collect(),
            max_output_tokens: 32,
            temperature: None,
            stream: true,
            response_schema: None,
            metadata: Value::Null,
        }
    }

    fn started_collector(request: &CanonicalInferenceRequest) -> CanonicalCollector {
        let mut collector = CanonicalCollector::default();
        collector
            .emit(CanonicalStreamEvent::ResponseStart {
                response_id: "resp-1".to_string(),
                model: request.model.clone(),
                created_at_seconds: 0,
            })
            .expect("response start");
        collector
    }

    fn complete_collector(
        mut collector: CanonicalCollector,
        request: &CanonicalInferenceRequest,
    ) -> M3HubResult<CanonicalInferenceResponse> {
        collector
            .emit(CanonicalStreamEvent::ResponseCompleted {
                response_id: "resp-1".to_string(),
                finish_reason: "tool_calls".to_string(),
                usage: CanonicalUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: None,
                },
            })
            .map_err(M3HubError::Runtime)?;
        collector.into_response(request, 0)
    }

    /// Splits the JSON text for `value` into 3+ fragments, deliberately
    /// cutting inside an embedded `{`/`}` pair and inside an escaped quote —
    /// the two spots a naive brace-counting parser (instead of buffering
    /// verbatim and parsing once complete) would get wrong. `value` must
    /// contain a string field with a `{...}` substring and an escaped quote.
    fn split_with_embedded_brace_and_escape(value: &Value) -> Vec<String> {
        let text = value.to_string();
        let mut braces = text.match_indices('{');
        let _outer_open = braces.next().expect("outer open brace");
        let (embedded_open, _) = braces.next().expect("embedded open brace");
        let (embedded_close, _) = text
            .match_indices('}')
            .next()
            .expect("embedded close brace");
        let (escape_at, _) = text.match_indices("\\\"").next().expect("escaped quote");
        let mut cuts = vec![embedded_open + 1, embedded_close, escape_at + 1];
        cuts.sort_unstable();
        cuts.dedup();
        let mut fragments = Vec::new();
        let mut previous = 0;
        for cut in cuts {
            fragments.push(text[previous..cut].to_string());
            previous = cut;
        }
        fragments.push(text[previous..].to_string());
        fragments
    }

    /// Naive brace-counting to find where a streamed tool call's JSON ends
    /// breaks the moment a string value contains braces. `CanonicalCollector`
    /// never brace-counts: it concatenates every `ToolCallArgumentsDelta`
    /// fragment verbatim and only asks `serde_json` to parse the result once
    /// the call is over, so this must reconstruct exactly.
    #[test]
    fn collector_reconstructs_brace_in_string_arguments_across_fragments() {
        let request = request_with_tools(&["search"]);
        let arguments_value = json!({"note": "find {ignored} \"quoted\" value"});
        let fragments = split_with_embedded_brace_and_escape(&arguments_value);
        assert!(fragments.len() >= 3, "expected multiple fragments");

        let mut collector = started_collector(&request);
        collector
            .emit(CanonicalStreamEvent::ToolCallStart {
                index: 0,
                call_id: "call_1".to_string(),
                name: "search".to_string(),
            })
            .expect("tool call start");
        for fragment in &fragments {
            collector
                .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    call_id: "call_1".to_string(),
                    json_delta: fragment.clone(),
                })
                .expect("argument fragment");
        }
        collector
            .emit(CanonicalStreamEvent::ToolCallEnd {
                index: 0,
                call_id: "call_1".to_string(),
            })
            .expect("tool call end");
        let response = complete_collector(collector, &request).expect("response assembles");
        assert_eq!(
            response.content,
            vec![CanonicalContent::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: arguments_value,
            }]
        );
    }

    #[test]
    fn collector_rejects_duplicate_tool_block_index() {
        let request = request_with_tools(&["search"]);
        let mut collector = started_collector(&request);
        collector
            .emit(CanonicalStreamEvent::ToolCallStart {
                index: 0,
                call_id: "call_1".to_string(),
                name: "search".to_string(),
            })
            .expect("first start");
        let result = collector.emit(CanonicalStreamEvent::ToolCallStart {
            index: 0,
            call_id: "call_2".to_string(),
            name: "search".to_string(),
        });
        assert!(matches!(result, Err(ref message) if message.contains("duplicate")));
    }

    #[test]
    fn collector_rejects_arguments_before_start_and_call_id_mismatch() {
        let request = request_with_tools(&["search"]);
        let mut collector = started_collector(&request);
        assert!(collector
            .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                call_id: "call_1".to_string(),
                json_delta: "{}".to_string(),
            })
            .is_err());

        collector
            .emit(CanonicalStreamEvent::ToolCallStart {
                index: 0,
                call_id: "call_1".to_string(),
                name: "search".to_string(),
            })
            .expect("start");
        let mismatched = collector.emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
            index: 0,
            call_id: "wrong-id".to_string(),
            json_delta: "{}".to_string(),
        });
        assert!(matches!(mismatched, Err(ref message) if message.contains("mismatch")));

        let end_mismatch = collector.emit(CanonicalStreamEvent::ToolCallEnd {
            index: 0,
            call_id: "wrong-id".to_string(),
        });
        assert!(matches!(end_mismatch, Err(ref message) if message.contains("mismatch")));
    }

    /// A tool call that started but never received `ToolCallEnd` (stream
    /// truncated, connection dropped) must not silently resolve into a
    /// completed response.
    #[test]
    fn collector_rejects_unfinished_tool_call_at_response_assembly() {
        let request = request_with_tools(&["search"]);
        let mut collector = started_collector(&request);
        collector
            .emit(CanonicalStreamEvent::ToolCallStart {
                index: 0,
                call_id: "call_1".to_string(),
                name: "search".to_string(),
            })
            .expect("start");
        collector
            .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                call_id: "call_1".to_string(),
                json_delta: "{\"q\": \"incomplete".to_string(),
            })
            .expect("delta");
        // No `ToolCallEnd`: the stream ends here.
        let result = complete_collector(collector, &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("unfinished")),
            "expected an unfinished-tool-call rejection, got {result:?}"
        );
    }

    #[test]
    fn collector_rejects_arguments_that_are_not_valid_json_or_not_an_object() {
        let request = request_with_tools(&["search"]);
        for arguments in ["{\"unterminated", "42", "[1,2,3]", ""] {
            let mut collector = started_collector(&request);
            collector
                .emit(CanonicalStreamEvent::ToolCallStart {
                    index: 0,
                    call_id: "call_1".to_string(),
                    name: "search".to_string(),
                })
                .expect("start");
            collector
                .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    call_id: "call_1".to_string(),
                    json_delta: arguments.to_string(),
                })
                .expect("delta");
            collector
                .emit(CanonicalStreamEvent::ToolCallEnd {
                    index: 0,
                    call_id: "call_1".to_string(),
                })
                .expect("end");
            let result = complete_collector(collector, &request);
            assert!(
                result.is_err(),
                "arguments {arguments:?} should not assemble into a response"
            );
        }
    }

    /// The other half of the acceptance criterion: a tool call naming
    /// something the request never offered must never reach a caller as a
    /// materialized `ToolUse` — the collector is the shared choke point for
    /// both the MLX and any future collector-backed engine, so it is the
    /// right place to enforce this regardless of which runtime produced the
    /// stream.
    #[test]
    fn collector_rejects_tool_call_naming_an_unoffered_tool() {
        let request = request_with_tools(&["weather"]);
        let mut collector = started_collector(&request);
        collector
            .emit(CanonicalStreamEvent::ToolCallStart {
                index: 0,
                call_id: "call_1".to_string(),
                name: "shell_exec".to_string(),
            })
            .expect("start");
        collector
            .emit(CanonicalStreamEvent::ToolCallArgumentsDelta {
                index: 0,
                call_id: "call_1".to_string(),
                json_delta: "{\"cmd\":\"rm -rf /\"}".to_string(),
            })
            .expect("delta");
        collector
            .emit(CanonicalStreamEvent::ToolCallEnd {
                index: 0,
                call_id: "call_1".to_string(),
            })
            .expect("end");
        let result = complete_collector(collector, &request);
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("shell_exec") && message.contains("not offered")),
            "expected an unoffered-tool rejection, got {result:?}"
        );
    }

    /// Full MLX-runtime pipeline: raw `MlxStreamEvent`s (as the MLX sidecar
    /// process would emit, one JSON object per line) run through
    /// `MlxCanonicalSink` and land in a `CanonicalCollector`, mirroring what
    /// `MlxRuntimeAdapter::stream`/`complete` do in production.
    #[cfg(target_os = "macos")]
    fn run_mlx_pipeline(
        request: &CanonicalInferenceRequest,
        events: Vec<MlxStreamEvent>,
    ) -> M3HubResult<CanonicalInferenceResponse> {
        let mut collector = started_collector(request);
        {
            let mut sink = MlxCanonicalSink::new(&mut collector, "resp-1".to_string());
            for event in events {
                sink.emit(event).map_err(M3HubError::Runtime)?;
            }
        }
        collector.into_response(request, 0)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_reconstructs_brace_in_string_arguments() {
        let request = request_with_tools(&["search"]);
        let arguments_value = json!({"note": "a {braced} \"quoted\" value"});
        let fragments = split_with_embedded_brace_and_escape(&arguments_value);
        assert!(fragments.len() >= 3, "expected multiple fragments");

        let mut events = vec![MlxStreamEvent::ToolCallStart {
            call_id: "call_1".to_string(),
            name: "search".to_string(),
        }];
        events.extend(fragments.into_iter().map(|fragment| {
            MlxStreamEvent::ToolCallArgumentsDelta {
                call_id: "call_1".to_string(),
                json: fragment,
            }
        }));
        events.push(MlxStreamEvent::ToolCallEnd {
            call_id: "call_1".to_string(),
        });
        events.push(MlxStreamEvent::Completed {
            input_tokens: 3,
            output_tokens: 5,
        });
        let response =
            run_mlx_pipeline(&request, events).expect("mlx pipeline assembles a response");
        assert_eq!(
            response.content,
            vec![CanonicalContent::ToolUse {
                id: "call_1".to_string(),
                name: "search".to_string(),
                input: arguments_value,
            }]
        );
        assert_eq!(response.finish_reason, "tool_use");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_rejects_duplicate_tool_call_id() {
        let request = request_with_tools(&["search"]);
        let result = run_mlx_pipeline(
            &request,
            vec![
                MlxStreamEvent::ToolCallStart {
                    call_id: "call_1".to_string(),
                    name: "search".to_string(),
                },
                MlxStreamEvent::ToolCallStart {
                    call_id: "call_1".to_string(),
                    name: "search".to_string(),
                },
            ],
        );
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("duplicate")),
            "expected a duplicate-id rejection, got {result:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_rejects_arguments_before_start() {
        let request = request_with_tools(&["search"]);
        let result = run_mlx_pipeline(
            &request,
            vec![MlxStreamEvent::ToolCallArgumentsDelta {
                call_id: "call_1".to_string(),
                json: "{}".to_string(),
            }],
        );
        assert!(matches!(result, Err(M3HubError::Runtime(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_rejects_end_for_unknown_tool_call() {
        let request = request_with_tools(&["search"]);
        let result = run_mlx_pipeline(
            &request,
            vec![MlxStreamEvent::ToolCallEnd {
                call_id: "call_1".to_string(),
            }],
        );
        assert!(matches!(result, Err(M3HubError::Runtime(_))));
    }

    /// The MLX sidecar process crashing or its stream being cut mid-call
    /// (started, never ended) must not silently complete as if nothing
    /// happened.
    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_rejects_completed_with_unfinished_tool_call() {
        let request = request_with_tools(&["search"]);
        let result = run_mlx_pipeline(
            &request,
            vec![
                MlxStreamEvent::ToolCallStart {
                    call_id: "call_1".to_string(),
                    name: "search".to_string(),
                },
                MlxStreamEvent::ToolCallArgumentsDelta {
                    call_id: "call_1".to_string(),
                    json: "{\"q\":\"incomplete".to_string(),
                },
                MlxStreamEvent::Completed {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        );
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("unfinished")),
            "expected an unfinished-tool-call rejection, got {result:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mlx_pipeline_rejects_tool_call_naming_an_unoffered_tool() {
        let request = request_with_tools(&["weather"]);
        let result = run_mlx_pipeline(
            &request,
            vec![
                MlxStreamEvent::ToolCallStart {
                    call_id: "call_1".to_string(),
                    name: "shell_exec".to_string(),
                },
                MlxStreamEvent::ToolCallArgumentsDelta {
                    call_id: "call_1".to_string(),
                    json: "{\"cmd\":\"rm -rf /\"}".to_string(),
                },
                MlxStreamEvent::ToolCallEnd {
                    call_id: "call_1".to_string(),
                },
                MlxStreamEvent::Completed {
                    input_tokens: 1,
                    output_tokens: 1,
                },
            ],
        );
        assert!(
            matches!(result, Err(M3HubError::Runtime(ref message)) if message.contains("shell_exec") && message.contains("not offered")),
            "expected an unoffered-tool rejection, got {result:?}"
        );
    }

    // ------------------------------------------------------------------
    // ROADMAP Phase 8 item 17: Sampler, Batching, and Speculative Decoding
    // Controls. Pure unit coverage for `ModelFamily::detect`,
    // `compatible_draft_models`, and `gate_advanced_settings` — the
    // end-to-end `set_runtime_config`/`load_model`/
    // `resolve_setting_capabilities` enforcement paths are covered by
    // `tests/m3_runtime_hub_contract.rs` against a real `M3RuntimeHub`.
    // ------------------------------------------------------------------

    fn installed_model_view(
        asset_id: &str,
        model_id: &str,
        display_name: &str,
        runtime: M3RuntimeKind,
        estimated_ram_bytes: u64,
    ) -> M3InstalledModelView {
        M3InstalledModelView {
            asset_id: asset_id.to_string(),
            model_id: model_id.to_string(),
            display_name: display_name.to_string(),
            runtime,
            variant_id: "q4_k_m".to_string(),
            capabilities: M3ModelCapabilities {
                chat: true,
                embeddings: false,
                tool_calling: false,
                vision: false,
                structured_output: false,
            },
            estimated_ram_bytes,
            estimated_vram_bytes: 0,
            required_accelerator: None,
            active_version_key: "version-1".to_string(),
            versions: Vec::new(),
        }
    }

    fn cpu_only_compatibility_report() -> M3HardwareCompatibilityReport {
        compatibility_report_from_snapshot(&HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
            available_ram_bytes: 8 * 1024 * 1024 * 1024,
            logical_cpu_count: 8,
            platform: crate::runtime_adapter::PlatformCapabilities::from_host(
                "linux",
                "x86_64",
                Vec::new(),
            ),
        })
    }

    fn cuda_compatibility_report() -> M3HardwareCompatibilityReport {
        compatibility_report_from_snapshot(&HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
            available_ram_bytes: 8 * 1024 * 1024 * 1024,
            logical_cpu_count: 8,
            platform: crate::runtime_adapter::PlatformCapabilities::from_host(
                "linux",
                "x86_64",
                vec![crate::runtime_adapter::AcceleratorCapability {
                    kind: AcceleratorKind::Cuda,
                    available: true,
                    device_names: vec!["Test GPU".to_string()],
                    total_memory_bytes: Some(24 * 1024 * 1024 * 1024),
                    available_memory_bytes: Some(20 * 1024 * 1024 * 1024),
                    devices: Vec::new(),
                }],
            ),
        })
    }

    /// Minimal capability fixture covering only the three keys
    /// `gate_advanced_settings` actually gates — deliberately not a copy of
    /// the real (private) `llama_setting_capabilities()`, so these tests
    /// exercise the gating function's contract rather than duplicating that
    /// module's own declaration.
    fn llama_capabilities_fixture() -> Vec<AdvancedSettingCapability> {
        vec![
            AdvancedSettingCapability {
                key: "flash_attention".to_string(),
                label: "Flash attention".to_string(),
                description: "Flash attention behavior".to_string(),
                schema: crate::runtime_adapter::SettingValueSchema::Choice {
                    options: vec!["auto".to_string(), "on".to_string(), "off".to_string()],
                },
                default_value: SettingValue::Choice {
                    value: "auto".to_string(),
                },
                restart_required: true,
                supported: true,
                unsupported_reason: None,
            },
            AdvancedSettingCapability {
                key: "mixed_precision".to_string(),
                label: "Mixed precision (KV cache)".to_string(),
                description: "KV cache quantization".to_string(),
                schema: crate::runtime_adapter::SettingValueSchema::Choice {
                    options: vec!["f16".to_string(), "q8_0".to_string(), "q4_0".to_string()],
                },
                default_value: SettingValue::Choice {
                    value: "f16".to_string(),
                },
                restart_required: true,
                supported: true,
                unsupported_reason: None,
            },
            AdvancedSettingCapability {
                key: "speculative_decoding_draft_model".to_string(),
                label: "Speculative decoding draft model".to_string(),
                description: "Draft model id for speculative decoding".to_string(),
                schema: crate::runtime_adapter::SettingValueSchema::Text { max_bytes: 256 },
                default_value: SettingValue::Text {
                    value: String::new(),
                },
                restart_required: true,
                supported: false,
                unsupported_reason: Some(
                    "Select a model to check for a compatible installed draft model.".to_string(),
                ),
            },
        ]
    }

    #[test]
    fn model_family_detect_buckets_known_names_and_falls_back_to_generic() {
        assert_eq!(
            ModelFamily::detect("llama-3-8b-instruct", "Llama 3 8B Instruct", "q4_k_m"),
            ModelFamily::Llama
        );
        assert_eq!(
            ModelFamily::detect("qwen2-7b-instruct", "Qwen2 7B Instruct", "q4_k_m"),
            ModelFamily::Qwen
        );
        assert_eq!(
            ModelFamily::detect("mixtral-8x7b", "Mixtral 8x7B", "q4_k_m"),
            ModelFamily::Mistral
        );
        assert_eq!(
            ModelFamily::detect("gemma-2-9b", "Gemma 2 9B", "q4_k_m"),
            ModelFamily::Gemma
        );
        assert_eq!(
            ModelFamily::detect("phi-3.5-mini", "Phi 3.5 Mini", "q4_k_m"),
            ModelFamily::Phi
        );
        assert_eq!(
            ModelFamily::detect("deepseek-r1-distill", "DeepSeek R1 Distill", "q4_k_m"),
            ModelFamily::DeepSeek
        );
        assert_eq!(
            ModelFamily::detect("custom-corp-model", "Custom Corp Model", "q4_k_m"),
            ModelFamily::Generic
        );
    }

    #[test]
    fn compatible_draft_models_requires_llama_cpp_named_family_match_and_smaller_footprint() {
        let target = installed_model_view(
            "llama_cpp:llama-3-8b-instruct:q4_k_m",
            "llama-3-8b-instruct",
            "Llama 3 8B Instruct",
            M3RuntimeKind::LlamaCpp,
            8_000_000_000,
        );
        let smaller_same_family = installed_model_view(
            "llama_cpp:llama-3-1b-instruct:q4_k_m",
            "llama-3-1b-instruct",
            "Llama 3 1B Instruct",
            M3RuntimeKind::LlamaCpp,
            1_000_000_000,
        );
        let larger_same_family = installed_model_view(
            "llama_cpp:llama-3-70b-instruct:q4_k_m",
            "llama-3-70b-instruct",
            "Llama 3 70B Instruct",
            M3RuntimeKind::LlamaCpp,
            70_000_000_000,
        );
        let smaller_different_family = installed_model_view(
            "llama_cpp:mistral-7b-instruct:q4_k_m",
            "mistral-7b-instruct",
            "Mistral 7B Instruct",
            M3RuntimeKind::LlamaCpp,
            4_000_000_000,
        );
        let smaller_same_family_wrong_runtime = installed_model_view(
            "ollama:llama-3-1b-instruct:q4_k_m",
            "llama-3-1b-instruct",
            "Llama 3 1B Instruct (Ollama)",
            M3RuntimeKind::Ollama,
            1_000_000_000,
        );
        let installed = vec![
            target.clone(),
            smaller_same_family.clone(),
            larger_same_family,
            smaller_different_family,
            smaller_same_family_wrong_runtime,
        ];

        let candidates = compatible_draft_models(&target, &installed);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].asset_id, smaller_same_family.asset_id);

        // A target whose own model_id/display_name never matched a named
        // family never gets a draft — two "generic" models are not assumed
        // related just because both are unclassified.
        let generic_target = installed_model_view(
            "llama_cpp:custom-corp-model:q4_k_m",
            "custom-corp-model",
            "Custom Corp Model",
            M3RuntimeKind::LlamaCpp,
            8_000_000_000,
        );
        let generic_smaller = installed_model_view(
            "llama_cpp:another-custom-model:q4_k_m",
            "another-custom-model",
            "Another Custom Model",
            M3RuntimeKind::LlamaCpp,
            1_000_000_000,
        );
        assert!(compatible_draft_models(
            &generic_target,
            &[generic_target.clone(), generic_smaller]
        )
        .is_empty());

        // A non-llama.cpp target never gets a draft at all, regardless of
        // family/size — speculative decoding is only wired for llama.cpp.
        let ollama_target = installed_model_view(
            "ollama:llama-3-8b-instruct:q4_k_m",
            "llama-3-8b-instruct",
            "Llama 3 8B Instruct",
            M3RuntimeKind::Ollama,
            8_000_000_000,
        );
        assert!(
            compatible_draft_models(&ollama_target, &installed_smaller_llama_cpp_fixture())
                .is_empty()
        );
    }

    /// A single smaller, same-family, llama.cpp-runtime model — used only to
    /// prove a non-llama.cpp target still gets no draft candidates even when
    /// a plausible one exists for a different runtime.
    fn installed_smaller_llama_cpp_fixture() -> Vec<M3InstalledModelView> {
        vec![installed_model_view(
            "llama_cpp:llama-3-1b-instruct:q4_k_m",
            "llama-3-1b-instruct",
            "Llama 3 1B Instruct",
            M3RuntimeKind::LlamaCpp,
            1_000_000_000,
        )]
    }

    #[test]
    fn gate_advanced_settings_flips_flash_attention_and_mixed_precision_with_the_hardware_report() {
        let capabilities = llama_capabilities_fixture();
        let cpu_result =
            gate_advanced_settings(&capabilities, &cpu_only_compatibility_report(), None, &[]);
        let flash_attention = cpu_result
            .settings
            .iter()
            .find(|setting| setting.key == "flash_attention")
            .expect("flash_attention present");
        assert!(!flash_attention.supported);
        assert!(flash_attention.unsupported_reason.is_some());
        let mixed_precision = cpu_result
            .settings
            .iter()
            .find(|setting| setting.key == "mixed_precision")
            .expect("mixed_precision present");
        assert!(!mixed_precision.supported);

        let gpu_result =
            gate_advanced_settings(&capabilities, &cuda_compatibility_report(), None, &[]);
        let flash_attention = gpu_result
            .settings
            .iter()
            .find(|setting| setting.key == "flash_attention")
            .expect("flash_attention present");
        assert!(flash_attention.supported);
        assert!(flash_attention.unsupported_reason.is_none());
        let mixed_precision = gpu_result
            .settings
            .iter()
            .find(|setting| setting.key == "mixed_precision")
            .expect("mixed_precision present");
        assert!(mixed_precision.supported);
    }

    #[test]
    fn gate_advanced_settings_reports_the_speculative_decoding_reason_with_and_without_a_target() {
        let capabilities = llama_capabilities_fixture();
        let report = cpu_only_compatibility_report();

        let no_target = gate_advanced_settings(&capabilities, &report, None, &[]);
        let draft_setting = no_target
            .settings
            .iter()
            .find(|setting| setting.key == "speculative_decoding_draft_model")
            .expect("speculative_decoding_draft_model present");
        assert!(!draft_setting.supported);
        assert_eq!(
            draft_setting.unsupported_reason.as_deref(),
            Some("Select a model to check for a compatible installed draft model.")
        );
        assert!(no_target.draft_model_candidates.is_empty());

        let target = installed_model_view(
            "llama_cpp:llama-3-8b-instruct:q4_k_m",
            "llama-3-8b-instruct",
            "Llama 3 8B Instruct",
            M3RuntimeKind::LlamaCpp,
            8_000_000_000,
        );
        let no_draft_installed =
            gate_advanced_settings(&capabilities, &report, Some(&target), &[target.clone()]);
        let draft_setting = no_draft_installed
            .settings
            .iter()
            .find(|setting| setting.key == "speculative_decoding_draft_model")
            .expect("speculative_decoding_draft_model present");
        assert!(!draft_setting.supported);
        assert!(draft_setting
            .unsupported_reason
            .as_deref()
            .is_some_and(|reason| reason.contains(&target.display_name)));
        assert!(no_draft_installed.draft_model_candidates.is_empty());

        let draft = installed_model_view(
            "llama_cpp:llama-3-1b-instruct:q4_k_m",
            "llama-3-1b-instruct",
            "Llama 3 1B Instruct",
            M3RuntimeKind::LlamaCpp,
            1_000_000_000,
        );
        let installed = vec![target.clone(), draft.clone()];
        let with_draft = gate_advanced_settings(&capabilities, &report, Some(&target), &installed);
        let draft_setting = with_draft
            .settings
            .iter()
            .find(|setting| setting.key == "speculative_decoding_draft_model")
            .expect("speculative_decoding_draft_model present");
        assert!(draft_setting.supported);
        assert!(draft_setting.unsupported_reason.is_none());
        assert_eq!(with_draft.draft_model_candidates.len(), 1);
        assert_eq!(
            with_draft.draft_model_candidates[0].model_id,
            draft.model_id
        );
    }

    /// The component feed is platform-agnostic, so this is what keeps a build
    /// from offering an install whose second half it does not carry. Asserted
    /// against the real `cfg!` rather than a fixture so it fails on whichever
    /// platform drifts: on macOS `mlx_runtime` must stay offered, and on Windows
    /// and Linux it must not, since `m3_mlx_install_component` is compiled only
    /// into the macOS build.
    #[test]
    fn only_macos_is_offered_an_mlx_runtime_component() {
        assert_eq!(
            component_kind_runs_here(M3ComponentKind::MlxRuntime),
            cfg!(target_os = "macos"),
            "an MLX component may only be offered where the MLX installer exists"
        );
        for kind in [
            M3ComponentKind::LlamaCppServer,
            M3ComponentKind::Tokenizer,
            M3ComponentKind::Converter,
            M3ComponentKind::ProjectorRuntime,
            M3ComponentKind::MetalSupport,
            M3ComponentKind::CudaSupport,
            M3ComponentKind::RocmSupport,
            M3ComponentKind::VulkanSupport,
            M3ComponentKind::StudioTool,
        ] {
            assert!(
                component_kind_runs_here(kind),
                "{kind:?} is not platform-gated and must stay offered everywhere"
            );
        }
    }

    /// The catalog this project actually publishes, byte-for-byte, as the MLX
    /// packaging workflow last wrote it. Pasted rather than constructed so these
    /// tests fail if the app stops being able to read the very document it ships a
    /// URL for.
    const PUBLISHED_CATALOG: &str = r#"[
  {
    "schemaVersion": 1,
    "sourceId": "little-monkey-mlx",
    "componentId": "mlx-runtime-apple-silicon",
    "kind": "mlx_runtime",
    "displayName": "MLX runtime (Apple silicon)",
    "accelerator": null,
    "version": "mlx-lm-0.28.4+py3.14",
    "channel": "beta",
    "downloadUrl": "https://github.com/AA-Box/little-monkey/releases/download/mlx-runtime-mlx-lm-0.28.4%2Bpy3.14/mlx-runtime-mlx-lm-0.28.4%2Bpy3.14.tar.gz",
    "sha256": "6adde291eb28e3bbd4190e73f9a1c70417581a89201860332cd5499ede302e0a",
    "sizeBytes": 70330595,
    "publishedAtMs": 1786182708413,
    "compatibilityNote": "Requires Apple silicon. Ships 5947 files.",
    "metadata": {}
  }
]"#;

    /// One artifact byte pattern, so an install test's success depends on the
    /// digest check actually passing rather than on it being skipped. Longer than
    /// one download chunk on purpose, so the install below reads several ranges.
    fn fixture_artifact() -> Vec<u8> {
        (0..196_608_u32).map(|index| (index % 251) as u8).collect()
    }

    /// The validator the fixture answers with, and the one a resumed range must
    /// present back to it.
    const FIXTURE_ETAG: &str = "\"fixture-etag\"";

    /// A pair of loopback listeners standing in for an origin and the CDN it
    /// redirects to.
    ///
    /// Two listeners and not one, because that is the difference between a test of
    /// cross-origin redirects and a test of path rewriting: `127.0.0.1:A` and
    /// `127.0.0.1:B` are different origins by the `(scheme, host, port)` rule, so a
    /// hop between them is exactly the hop `egress::hardened`'s same-origin policy
    /// refuses and the one a release asset requires. Responses are written by hand
    /// because what is under test is status lines, `Location` headers and which
    /// request headers survived, not a server framework.
    struct RedirectFixture {
        origin: String,
    }

    impl RedirectFixture {
        async fn spawn() -> Self {
            let assets = Self::serve(None).await;
            let origin = Self::serve(Some(assets.clone())).await;
            Self { origin }
        }

        /// `assets` is the base a redirect points at: the origin listener redirects
        /// to the asset listener, and the asset listener answers.
        async fn serve(assets: Option<String>) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind fixture");
            let address = listener.local_addr().expect("fixture address");
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = listener.accept().await {
                    let assets = assets.clone();
                    tokio::spawn(async move {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        // Drained before answering: a response written onto an
                        // unread request closes with an RST on macOS, which the
                        // client reports as a transport error rather than as the
                        // status the test meant to assert on.
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            match stream.read(&mut buffer).await {
                                Ok(0) | Err(_) => return,
                                Ok(read) => request.extend_from_slice(&buffer[..read]),
                            }
                        }
                        let request = String::from_utf8_lossy(&request).to_string();
                        let mut lines = request.lines();
                        let start = lines.next().unwrap_or_default().to_string();
                        let mut parts = start.split(' ');
                        let method = parts.next().unwrap_or("GET").to_string();
                        let path = parts.next().unwrap_or("/").to_string();
                        let header = |name: &str| {
                            request
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case(name)
                                        .then(|| value.trim().to_string())
                                })
                                .unwrap_or_default()
                        };
                        // Every response says `connection: close`, because this
                        // fixture serves one request per connection and then drops
                        // it. Without saying so it looks like a keep-alive server,
                        // and the hop-limit test — four requests to one origin in a
                        // row — fails on a pooled socket the fixture had already
                        // closed rather than on the hop limit under test. It passed
                        // alone and failed in a loaded suite, which is the shape
                        // that costs an afternoon.
                        let redirect = |status: &str, location: &str| {
                            format!(
                                "HTTP/1.1 {status}\r\nlocation: {location}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                            )
                        };
                        let body = |status: &str, extra: &str, body: &[u8]| {
                            let mut response = format!(
                                "HTTP/1.1 {status}\r\nconnection: close\r\n{extra}content-length: {}\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            if method != "HEAD" {
                                response.extend_from_slice(body);
                            }
                            response
                        };
                        let artifact = fixture_artifact();
                        let response = match (assets.as_deref(), path.as_str()) {
                            // The origin listener: every path here answers with a
                            // hop to the *other* listener, which is what makes the
                            // redirect cross-origin.
                            (Some(assets), "/catalog.json") => {
                                redirect("302 Found", &format!("{assets}/catalog.json")).into_bytes()
                            }
                            (Some(assets), "/artifact.bin") => {
                                redirect("302 Found", &format!("{assets}/artifact.bin")).into_bytes()
                            }
                            // Every redirect status a `GET`/`HEAD` may carry, so the
                            // claim is about the class and not about `302`.
                            (Some(assets), "/moved") => {
                                redirect("301 Moved Permanently", &format!("{assets}/catalog.json"))
                                    .into_bytes()
                            }
                            (Some(assets), "/see-other") => {
                                redirect("303 See Other", &format!("{assets}/catalog.json"))
                                    .into_bytes()
                            }
                            (Some(assets), "/temporary") => {
                                redirect("307 Temporary Redirect", &format!("{assets}/catalog.json"))
                                    .into_bytes()
                            }
                            (Some(assets), "/permanent") => {
                                redirect("308 Permanent Redirect", &format!("{assets}/catalog.json"))
                                    .into_bytes()
                            }
                            // Relative, so the resolution against the URL that
                            // answered is exercised rather than assumed.
                            (Some(_), "/relative") => {
                                redirect("302 Found", "./catalog.json").into_bytes()
                            }
                            (Some(_), "/loop") => redirect("302 Found", "/loop").into_bytes(),
                            // A `3xx` reqwest cannot follow. It must not reach a
                            // caller as anything that could pass for success.
                            (Some(_), "/no-location") => {
                                b"HTTP/1.1 302 Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                                    .to_vec()
                            }
                            (Some(_), "/credentials") => {
                                redirect("302 Found", "https://user:secret@public.invalid/catalog.json")
                                    .into_bytes()
                            }
                            (Some(_), "/fragment") => {
                                redirect("302 Found", "https://public.invalid/catalog.json#frag")
                                    .into_bytes()
                            }
                            (Some(_), "/cleartext") => {
                                redirect("302 Found", "http://public.invalid/catalog.json").into_bytes()
                            }
                            // The asset listener.
                            (None, "/catalog.json") => body(
                                "200 OK",
                                "content-type: application/json\r\n",
                                PUBLISHED_CATALOG.as_bytes(),
                            ),
                            (None, "/artifact.bin") => {
                                let range = header("range");
                                let if_range = header("if-range");
                                if method == "HEAD" || range.is_empty() {
                                    // No range: the whole artifact, exactly as a
                                    // real asset host answers. `read_range` requires
                                    // `206`, so a hop that lost the `Range` header
                                    // fails here rather than quietly downloading
                                    // everything.
                                    body(
                                        "200 OK",
                                        "accept-ranges: bytes\r\netag: \"fixture-etag\"\r\n",
                                        &artifact,
                                    )
                                } else if !if_range.is_empty() && if_range != FIXTURE_ETAG {
                                    // A validator that does not match is a refusal,
                                    // which is what `If-Range` is *for*. Answering
                                    // `206` here regardless would make the header's
                                    // survival unobservable, and an unobservable
                                    // header is one a refactor can drop.
                                    b"HTTP/1.1 412 Precondition Failed\r\nconnection: close\r\ncontent-length: 0\r\n\r\n".to_vec()
                                } else {
                                    let (start, end) = range
                                        .trim_start_matches("bytes=")
                                        .split_once('-')
                                        .unwrap_or(("0", "0"));
                                    let start: usize = start.parse().unwrap_or(0);
                                    let end: usize = end
                                        .parse()
                                        .unwrap_or(artifact.len() - 1)
                                        .min(artifact.len() - 1);
                                    body(
                                        "206 Partial Content",
                                        &format!(
                                            "content-range: bytes {start}-{end}/{}\r\netag: {FIXTURE_ETAG}\r\n",
                                            artifact.len(),
                                        ),
                                        &artifact[start..=end],
                                    )
                                }
                            }
                            _ => b"HTTP/1.1 404 Not Found\r\nconnection: close\r\ncontent-length: 0\r\n\r\n"
                                .to_vec(),
                        };
                        let _ = stream.write_all(&response).await;
                        let _ = stream.flush().await;
                    });
                }
            });
            format!("http://{address}")
        }
    }

    /// The whole point of the fetch path: a published catalog answers the only
    /// stable URL it has with a redirect to a different origin, and the app has to
    /// end up holding the entries anyway. Before this, a `302` was an error and the
    /// sole way to reach a published component was to download the JSON in a
    /// browser and import the file by hand.
    #[tokio::test]
    async fn a_published_catalog_is_fetched_through_a_cross_origin_redirect() {
        let fixture = RedirectFixture::spawn().await;
        let context = M3OperationContext::default();
        for path in [
            "/catalog.json",
            "/moved",
            "/see-other",
            "/temporary",
            "/permanent",
            "/relative",
        ] {
            let entries = fetch_component_catalog(&format!("{}{path}", fixture.origin), &context)
                .await
                .unwrap_or_else(|error| panic!("{path} must be followed: {error}"));
            assert_eq!(entries.len(), 1, "{path}");
            assert_eq!(entries[0].component_id, "mlx-runtime-apple-silicon");
            assert_eq!(entries[0].version, "mlx-lm-0.28.4+py3.14");
            assert_eq!(entries[0].kind, M3ComponentKind::MlxRuntime);
        }
    }

    /// A redirect this app refuses or cannot follow must never look like a fetch
    /// that worked. Each of these is a shape a hostile or broken `Location` could
    /// take, and the assertion is on the *reason*, because "download failed" for all
    /// of them is what made the old code impossible to debug.
    #[tokio::test]
    async fn a_redirect_that_cannot_be_followed_is_refused_by_name() {
        let fixture = RedirectFixture::spawn().await;
        let context = M3OperationContext::default();
        for (path, expected) in [
            ("/loop", "more than"),
            ("/no-location", "without a Location"),
            (
                "/credentials",
                crate::egress::EgressRule::EmbeddedCredentials.code(),
            ),
            ("/fragment", "error sending request"),
            (
                "/cleartext",
                crate::egress::EgressRule::SchemeNotAllowed.code(),
            ),
        ] {
            let error = fetch_component_catalog(&format!("{}{path}", fixture.origin), &context)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(expected),
                "{path} should have reported {expected}, said: {error}"
            );
        }
    }

    /// The artifact side of the same hop, and the reason a hand-rolled follower was
    /// the wrong tool: a release asset is downloaded in ranges, and a hop that
    /// dropped the range would be answered `200` with all 70 MB. `read_range`
    /// requires `206`, so that failure surfaces as "byte-range support is required"
    /// against a server that supports it perfectly well. `If-Range` matters for the
    /// same reason one step later: it is what makes a *resumed* download safe.
    #[tokio::test]
    async fn a_redirected_range_request_keeps_its_range_and_validator() {
        let fixture = RedirectFixture::spawn().await;
        let context = M3OperationContext::default();
        let transport = ReqwestM3DownloadTransport::for_loopback_fixture().expect("transport");
        let url = format!("{}/artifact.bin", fixture.origin);

        // `HEAD` after the hop, which is what a real release asset answers — probed
        // for real rather than assumed, because if it did not this path would need a
        // ranged fallback probe instead.
        let probe = transport.probe(&url, &context).await.expect("probe");
        assert_eq!(probe.total_bytes, fixture_artifact().len() as u64);
        assert_eq!(probe.etag.as_deref(), Some(FIXTURE_ETAG));
        assert!(
            probe.accepts_ranges,
            "HEAD survived the hop with its headers"
        );

        // `206` and these exact bytes is the proof: had the `Range` header not
        // survived the hop, the fixture would have answered `200` with the whole
        // artifact and `read_range` would have failed for want of byte-range
        // support. Had `If-Range` not survived, the fixture — which refuses a
        // mismatched validator, as `If-Range` exists to do — would have answered
        // `412`.
        let chunk = transport
            .read_range(&url, 16, 64, probe.etag.as_deref(), &context)
            .await
            .expect("a redirected range request keeps its Range and If-Range");
        assert_eq!(chunk.offset, 16);
        assert_eq!(chunk.bytes, fixture_artifact()[16..80]);
        assert_eq!(chunk.total_bytes, fixture_artifact().len() as u64);

        // The negative half, so the two assertions above cannot both be passing for
        // the wrong reason: a validator the asset does not match is refused, and the
        // refusal reaches the caller.
        let stale = transport
            .read_range(&url, 16, 64, Some("\"someone-elses-etag\""), &context)
            .await
            .expect_err("a mismatched If-Range must not yield bytes");
        assert!(stale.to_string().contains("412"), "{stale}");
    }

    /// The whole ladder, in one test, because every rung of it was reachable only in
    /// theory before: a published catalog URL that redirects, the entries adopted
    /// into a registry, an install that probes and reads ranges through a second
    /// redirect, and the size and digest checks that decide whether any of it counts.
    ///
    /// `llama_cpp_server` rather than `mlx_runtime`, for two reasons. Its artifact is
    /// a single blob, so the install completes on any platform instead of needing
    /// macOS and a signed package — and the MLX signature path keeps its own tests,
    /// which is where it belongs: a fake signed package built here would be a
    /// production signature check tested against a fixture that agrees with it.
    #[tokio::test]
    async fn a_registered_component_installs_from_a_redirecting_url() {
        let fixture = RedirectFixture::spawn().await;
        let artifact = fixture_artifact();
        let entry = M3ComponentCatalogEntry {
            download_url: format!("{}/artifact.bin", fixture.origin),
            sha256: sha256_hex(&artifact),
            size_bytes: artifact.len() as u64,
            ..registry_fixture_entry()
        };
        let root =
            std::env::temp_dir().join(format!("little-monkey-component-e2e-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("component root");
        let hub = M3ComponentHub::new(
            &root,
            M3HubConfig {
                download_chunk_bytes: 65_536,
                ..M3HubConfig::default()
            },
            M3ComponentHubDependencies {
                clock: Arc::new(SystemM3Clock),
                download: Arc::new(
                    ReqwestM3DownloadTransport::for_loopback_fixture().expect("transport"),
                ),
                sources: vec![Arc::new(
                    StaticM3ComponentSource::new("local", vec![entry.clone()])
                        .expect("registry source"),
                )],
            },
        )
        .expect("component hub");
        let context = M3OperationContext::default();

        let listed = hub.list_registry(&context).await.expect("list registry");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].registry_key(), entry.registry_key());

        let installed = hub
            .install_component(
                &M3InstallComponentRequest {
                    entry: entry.clone(),
                },
                &context,
            )
            .await
            .expect("a redirecting URL installs");
        assert_eq!(installed.component_id, entry.component_id);
        let active = installed
            .versions
            .iter()
            .find(|version| version.active)
            .expect("an installed component has an active version");
        assert_eq!(active.sha256, entry.sha256);
        assert_eq!(
            std::fs::read(&active.artifact_path).expect("read installed artifact"),
            artifact,
            "the bytes on disk are the bytes the digest names"
        );
        assert!(
            hub.list_installed()
                .expect("list installed")
                .iter()
                .any(|component| component.component_id == entry.component_id),
            "an installed component has to show up as installed"
        );

        // The digest is what decides, and it decides against the registry's claim
        // rather than against the response: a catalog naming the right URL and the
        // wrong digest installs nothing.
        let wrong = M3ComponentCatalogEntry {
            component_id: "llama-cpp-server-vulkan".to_string(),
            sha256: "c".repeat(64),
            ..entry.clone()
        };
        let error = hub
            .install_component(&M3InstallComponentRequest { entry: wrong }, &context)
            .await
            .expect_err("a digest mismatch must refuse the install");
        assert!(
            error.to_string().to_ascii_lowercase().contains("sha256")
                || error.to_string().to_ascii_lowercase().contains("digest")
                || error.to_string().to_ascii_lowercase().contains("checksum"),
            "{error}"
        );

        // And so does the declared size, which is checked at the probe before a byte
        // is read.
        let short = M3ComponentCatalogEntry {
            component_id: "llama-cpp-server-cuda".to_string(),
            size_bytes: artifact.len() as u64 - 1,
            ..entry.clone()
        };
        let error = hub
            .install_component(&M3InstallComponentRequest { entry: short }, &context)
            .await
            .expect_err("a size mismatch must refuse the install");
        assert!(
            error.to_string().contains("bytes but server declares"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A fetched catalog is adopted whole or not at all, so one malformed entry
    /// cannot leave a partial registry behind — and the refusal happens before
    /// anything is written, which is what makes "not at all" true rather than
    /// aspirational.
    #[test]
    fn a_catalog_with_any_invalid_entry_is_refused_entirely() {
        let mut entries: Vec<serde_json::Value> =
            serde_json::from_str(PUBLISHED_CATALOG).expect("parse fixture");
        let mut broken = entries[0].clone();
        broken["sha256"] = serde_json::json!("not-a-digest");
        entries.push(broken);
        let document = serde_json::to_vec(&entries).expect("serialize");
        assert!(parse_component_catalog(&document).is_err());
    }

    #[test]
    fn a_catalog_is_read_as_a_bare_array_or_as_a_registry_export() {
        let array = parse_component_catalog(PUBLISHED_CATALOG.as_bytes()).expect("bare array");
        let envelope = parse_component_catalog(
            format!(r#"{{"schemaVersion":1,"entries":{PUBLISHED_CATALOG}}}"#).as_bytes(),
        )
        .expect("registry export");
        assert_eq!(array, envelope);
    }

    #[test]
    fn a_catalog_listing_more_entries_than_the_cap_is_refused() {
        let entry: serde_json::Value = serde_json::from_str(PUBLISHED_CATALOG)
            .map(|entries: Vec<serde_json::Value>| entries[0].clone())
            .expect("parse fixture");
        let over = vec![entry; MAX_CATALOG_ENTRIES + 1];
        let error = parse_component_catalog(&serde_json::to_vec(&over).expect("serialize"))
            .expect_err("the cap is a cap");
        assert!(error.to_string().contains("at most"), "{error}");
    }

    #[test]
    fn an_initial_component_download_url_must_be_public() {
        for url in [
            "https://127.0.0.1/component.tar.gz",
            "https://10.0.0.1/component.tar.gz",
            "https://100.64.0.1/component.tar.gz",
            "https://198.18.0.1/component.tar.gz",
        ] {
            assert!(validate_download_url(url, false).is_err(), "{url}");
        }
    }

    /// The body cap has to hold before the allocation, not after it: a peer that
    /// declares nothing and streams forever must be cut by our limit rather than by
    /// its own honesty. This drives the same `read_response_bounded` the catalog
    /// fetch uses.
    #[tokio::test]
    async fn an_oversized_catalog_body_is_refused_without_being_buffered() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer).await;
                // Chunked with no declared length, so the only thing that can stop
                // this is the reader's own cap.
                if stream
                    .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
                    .await
                    .is_err()
                {
                    continue;
                }
                let filler = format!("{:x}\r\n{}\r\n", 64 * 1024, "x".repeat(64 * 1024));
                while stream.write_all(filler.as_bytes()).await.is_ok() {}
            }
        });
        let context = M3OperationContext::default();
        let error = fetch_component_catalog(&format!("http://{address}/catalog.json"), &context)
            .await
            .expect_err("an unbounded body must be refused");
        assert!(error.to_string().contains("body limit"), "{error}");
    }

    /// Discovery is not authenticity, and this is the line in code that says so.
    ///
    /// A SHA-256 the catalog supplied proves the bytes are the ones that catalog
    /// meant, which is worth nothing against whoever can rewrite the catalog. Only
    /// `mlx_runtime` installs through a pinned publisher key today, so only
    /// `mlx_runtime` may arrive this way — and the refusal names the manual path
    /// rather than pretending the kind does not exist.
    #[tokio::test]
    async fn a_fetched_catalog_may_only_list_kinds_whose_install_verifies_a_publisher_key() {
        let mut entry: serde_json::Value = serde_json::from_str(PUBLISHED_CATALOG)
            .map(|entries: Vec<serde_json::Value>| entries[0].clone())
            .expect("parse fixture");
        entry["kind"] = serde_json::json!("llama_cpp_server");
        entry["componentId"] = serde_json::json!("llama-cpp-server-metal");
        // Reaches the kind check only because everything else about it is valid,
        // which is the case that matters: a well-formed catalog listing an
        // unverifiable executable.
        let document = serde_json::to_vec(&vec![entry]).expect("serialize");
        parse_component_catalog(&document).expect("the entry itself is valid");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let document = document.clone();
                let mut buffer = [0_u8; 1024];
                let _ = stream.read(&mut buffer).await;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: {}\r\n\r\n",
                    document.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&document).await;
            }
        });
        let context = M3OperationContext::default();
        let error = fetch_component_catalog(&format!("http://{address}/catalog.json"), &context)
            .await
            .expect_err("an unsigned executable kind must not arrive over the network")
            .to_string();
        assert!(error.contains("pinned publisher key"), "{error}");
        assert!(error.contains("Import catalog"), "{error}");

        // And the rule is exhaustive rather than a list someone has to remember to
        // extend: every kind answers, and only one answers yes today.
        let verified: Vec<M3ComponentKind> = [
            M3ComponentKind::LlamaCppServer,
            M3ComponentKind::MlxRuntime,
            M3ComponentKind::Tokenizer,
            M3ComponentKind::Converter,
            M3ComponentKind::ProjectorRuntime,
            M3ComponentKind::MetalSupport,
            M3ComponentKind::CudaSupport,
            M3ComponentKind::RocmSupport,
            M3ComponentKind::VulkanSupport,
            M3ComponentKind::StudioTool,
        ]
        .into_iter()
        .filter(|kind| kind_verifies_publisher_signature(*kind))
        .collect();
        assert_eq!(verified, vec![M3ComponentKind::MlxRuntime]);
    }

    /// The registry's identity, and the field it was missing.
    ///
    /// `component_id` + `version` + `sha256` let two publishers' entries for the
    /// same version collide, so adopting one could overwrite the other while
    /// silently moving where the bytes come from. The URL is in the key, so those
    /// are two rows. `entryKey` in `RuntimeHubComponents.test.ts` pins the same four
    /// fields on the frontend side, which is what keeps the React key and the
    /// registry row meaning the same thing.
    #[test]
    fn a_registry_key_is_the_one_identity_the_registry_merges_on() {
        let base = registry_fixture_entry();
        for mutate in [
            (|entry: &mut M3ComponentCatalogEntry| entry.component_id = "other".to_string())
                as fn(&mut M3ComponentCatalogEntry),
            |entry| entry.version = "other".to_string(),
            |entry| entry.sha256 = "b".repeat(64),
            |entry| entry.download_url = "https://components.example.test/elsewhere".to_string(),
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(changed.registry_key(), base.registry_key());
        }
        // And nothing else. `source_id` especially: the local registry restamps it
        // on adoption, so a key that read it would change under a row's feet.
        for mutate in [
            (|entry: &mut M3ComponentCatalogEntry| {
                entry.source_id = "little-monkey-mlx".to_string()
            }) as fn(&mut M3ComponentCatalogEntry),
            |entry| entry.display_name = "Renamed".to_string(),
            |entry| entry.compatibility_note = Some("needs macOS 15".to_string()),
            |entry| entry.published_at_ms = 1_800_000_000_000,
        ] {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_eq!(changed.registry_key(), base.registry_key());
        }
    }

    fn registry_fixture_entry() -> M3ComponentCatalogEntry {
        M3ComponentCatalogEntry {
            schema_version: M3_COMPONENT_CATALOG_SCHEMA_VERSION,
            source_id: "local".to_string(),
            component_id: "llama-cpp-server-metal".to_string(),
            kind: M3ComponentKind::LlamaCppServer,
            display_name: "llama.cpp server (Metal)".to_string(),
            accelerator: None,
            version: "b4100".to_string(),
            channel: M3ComponentChannel::Stable,
            download_url: "https://components.example.test/llama-server".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 1024,
            published_at_ms: 1_700_000_000_000,
            compatibility_note: None,
            metadata: BTreeMap::new(),
        }
    }

    /// The published catalog URL, fetched for real.
    ///
    /// Ignored by default and deliberately: normal CI must not fail because GitHub
    /// is having a bad morning, and nothing here downloads the 70 MB runtime. What
    /// it proves is the one thing a fixture cannot — that the stable URL this app
    /// ships still answers, that its real redirect chain is accepted by the real
    /// public-destination rule, and that the document on the other side still
    /// parses into the component this project publishes.
    ///
    /// Run with `cargo test -- --ignored the_published_catalog_url`.
    #[tokio::test]
    #[ignore = "reaches github.com; run deliberately"]
    async fn the_published_catalog_url_is_reachable_and_parses() {
        let context = M3OperationContext::default();
        let entries = fetch_component_catalog(
            crate::m3_production::DEFAULT_COMPONENT_CATALOG_URL,
            &context,
        )
        .await
        .expect("the published catalog is reachable through its redirect");
        assert!(
            entries
                .iter()
                .any(|entry| entry.kind == M3ComponentKind::MlxRuntime),
            "the published catalog no longer lists the MLX runtime: {entries:#?}"
        );
    }
}
